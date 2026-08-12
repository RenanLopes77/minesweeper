use crate::{Board, Event, Mode, Reveal, Status};

pub struct Game {
    pub board: Board,
    pub mode: Mode,
    seed: u64,
    placed: bool,
}

impl Game {
    pub fn new(seed: u64, w: u8, h: u8, mines: u16, mode: Mode) -> Self {
        Game {
            board: Board::new(w, h, mines),
            mode,
            seed,
            placed: false,
        }
    }

    pub fn apply(&mut self, ev: &Event) {
        // Start is checked before the status guard below: restarting is
        // exactly what you do from a finished game.
        if let Event::Start {
            seed,
            w,
            h,
            mines,
            mode,
        } = *ev
        {
            *self = Game::new(seed, w, h, mines, mode);
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
                    // Two racers hold two boards that have to be the *same*
                    // board, so the layout cannot depend on who clicked where
                    // first. The centre is the agreed safe opening instead;
                    // every other first click is a real risk, equally, for
                    // both of them.
                    let opening = match self.mode {
                        Mode::Race => (self.board.w / 2, self.board.h / 2),
                        _ => (x, y),
                    };
                    self.board.place_mines(self.seed, opening);
                    self.placed = true;
                }
                // In a flag race the mines are the prize, not the punishment:
                // uncovering one claims it. `player` is not stored — the flag
                // is on the board and the log says who put it there.
                if self.mode == Mode::FlagRace && self.board.get(x, y).mine {
                    self.board.claim(x, y);
                    return;
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

    /// A flag race ends when the last mine is claimed — nobody can lose it,
    /// so the board's own "did anyone step on a mine" rule never fires and
    /// its "is everything safe uncovered" rule would drag the game past the
    /// point where the result is settled.
    pub fn status(&self) -> Status {
        if self.mode == Mode::FlagRace {
            let claimed = self
                .board
                .cells()
                .iter()
                .filter(|c| c.mine && c.state == Reveal::Flagged)
                .count();
            return if claimed as u16 == self.board.mines {
                Status::Won
            } else {
                Status::Playing
            };
        }
        self.board.status()
    }

    pub fn replay(events: &[Event]) -> Option<Game> {
        let Event::Start {
            seed,
            w,
            h,
            mines,
            mode,
        } = *events.first()?
        else {
            return None;
        };
        let mut g = Game::new(seed, w, h, mines, mode);
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
                mode: Mode::Coop,
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
        let mut g = Game::new(7, 9, 9, 10, Mode::Coop);
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
        let mut g = Game::new(7, 9, 9, 10, Mode::Coop);
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
        let mut g = Game::new(7, 9, 9, 10, Mode::Coop);
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
        let mut g = Game::new(7, 9, 9, 10, Mode::Coop);
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
            mode: Mode::Coop,
        });
        assert_eq!(
            g.status(),
            Status::Playing,
            "could not restart after losing"
        );
    }

    fn start() -> Event {
        Event::Start {
            seed: 11,
            w: 9,
            h: 9,
            mines: 10,
            mode: Mode::Coop,
        }
    }

    /// **The first reveal decides the whole board.** Mines are placed on the
    /// first Reveal, using that cell as the safe zone — so two peers whose
    /// first reveals differ do not merely disagree about one cell, they are
    /// playing different games.
    ///
    /// In practice the host ships its log on connect, so both sides share a
    /// first reveal. The hazard is both peers opening their first cell before
    /// either has heard from the other.
    #[test]
    fn the_first_reveal_decides_the_board() {
        let p = Event::Reveal {
            player: 0,
            x: 4,
            y: 4,
        };
        let q = Event::Reveal {
            player: 1,
            x: 7,
            y: 2,
        };
        let a = Game::replay(&[start(), p, q]).unwrap();
        let b = Game::replay(&[start(), q, p]).unwrap();
        assert_ne!(
            a.hash(),
            b.hash(),
            "if these agree, placement no longer depends on the first click"
        );
    }

    /// A losing move silences every move after it, so if one peer's ordering
    /// puts the mine first and the other's puts it last, the boards differ.
    ///
    /// Third and last of the ordering hazards, after the opening reveal and
    /// same-cell reveal/flag. All three are why ch14 exists.
    #[test]
    fn a_lost_game_makes_order_matter() {
        let opened = Event::Reveal {
            player: 0,
            x: 0,
            y: 0,
        };
        let base = Game::replay(&[start(), opened]).unwrap();

        let cells = || (0..9u8).flat_map(|y| (0..9u8).map(move |x| (x, y)));
        let (mx, my) = cells().find(|&(x, y)| base.board.get(x, y).mine).unwrap();
        let (sx, sy) = cells()
            .find(|&(x, y)| {
                let c = base.board.get(x, y);
                !c.mine && c.state == crate::Reveal::Hidden
            })
            .unwrap();

        let boom = Event::Reveal {
            player: 0,
            x: mx,
            y: my,
        };
        let safe = Event::Reveal {
            player: 1,
            x: sx,
            y: sy,
        };

        let a = Game::replay(&[start(), opened, boom, safe]).unwrap();
        let b = Game::replay(&[start(), opened, safe, boom]).unwrap();
        assert_eq!(a.status(), Status::Lost);
        assert_eq!(b.status(), Status::Lost);
        assert_ne!(a.hash(), b.hash(), "the safe cell opened in only one order");
    }

    /// Flags are toggles, so applying both peers' flags in either order lands
    /// in the same place.
    #[test]
    fn flags_in_either_order_agree() {
        let start = Event::Start {
            seed: 11,
            w: 9,
            h: 9,
            mines: 10,
            mode: Mode::Coop,
        };
        let p = Event::Flag {
            player: 0,
            x: 1,
            y: 1,
        };
        let q = Event::Flag {
            player: 1,
            x: 5,
            y: 5,
        };
        let a = Game::replay(&[start, p, q]).unwrap();
        let b = Game::replay(&[start, q, p]).unwrap();
        assert_eq!(a.hash(), b.hash());
    }

    /// ...but a reveal and a flag on the SAME cell do not commute, and this is
    /// the one case that can genuinely desync two peers.
    ///
    /// Reveal-then-flag: the cell opens, and the flag is ignored because it is
    /// already Shown. Flag-then-reveal: the cell is Flagged, and `reveal` skips
    /// anything that is not Hidden — so nothing opens at all.
    ///
    /// Two peers clicking the same cell at the same moment, one revealing and
    /// one flagging, each apply their own first. They end up with different
    /// boards. Nothing here fixes that; the hash comparison in the shell exists
    /// to *notice* it, and ch14's ordering is what will prevent it.
    #[test]
    fn reveal_and_flag_on_one_cell_do_not_commute() {
        let start = Event::Start {
            seed: 11,
            w: 9,
            h: 9,
            mines: 10,
            mode: Mode::Coop,
        };
        let reveal = Event::Reveal {
            player: 0,
            x: 4,
            y: 4,
        };
        let flag = Event::Flag {
            player: 1,
            x: 4,
            y: 4,
        };
        let a = Game::replay(&[start, reveal, flag]).unwrap();
        let b = Game::replay(&[start, flag, reveal]).unwrap();
        assert_ne!(
            a.hash(),
            b.hash(),
            "if these ever agree, ordering has been fixed — update this test"
        );
    }

    proptest! {
        /// Reveals commute at any size, *given the board already exists*. This
        /// is what makes ordinary simultaneous play safe with no ordering
        /// protocol: only the opening move and same-cell reveal/flag races
        /// need ch14.
        #[test]
        fn reveals_after_placement_agree_in_any_order(
            seed: u64,
            moves in prop::collection::vec((0u8..9, 0u8..9), 0..20),
        ) {
            let start = Event::Start { seed, w: 9, h: 9, mines: 10, mode: Mode::Coop };
            // Fixes the mine layout before the moves under test.
            let opened = Event::Reveal { player: 0, x: 0, y: 0 };
            let evs: Vec<Event> = moves
                .iter()
                .map(|&(x, y)| Event::Reveal { player: 0, x, y })
                .collect();

            let mut forward = vec![start, opened];
            forward.extend(evs.iter().copied());
            let mut backward = vec![start, opened];
            backward.extend(evs.iter().rev().copied());

            let a = Game::replay(&forward).unwrap();
            let b = Game::replay(&backward).unwrap();

            // Only meaningful while nobody has lost: a mine ends the game and
            // silences whatever came after it, which is order-dependent by
            // design. That case is covered by a_lost_game_makes_order_matter.
            if a.status() == Status::Playing && b.status() == Status::Playing {
                prop_assert_eq!(a.hash(), b.hash());
            }
        }

        /// The property that makes peer-to-peer safe: same log in, same hash out.
        #[test]
        fn identical_logs_agree(
            seed: u64,
            moves in prop::collection::vec((0u8..9, 0u8..9), 0..40),
        ) {
            let mut log = vec![Event::Start { seed, w: 9, h: 9, mines: 10, mode: Mode::Coop }];
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

#[cfg(test)]
mod mode_tests {
    use super::*;

    fn mines_of(g: &Game) -> Vec<(u8, u8)> {
        (0..g.board.h)
            .flat_map(|y| (0..g.board.w).map(move |x| (x, y)))
            .filter(|&(x, y)| g.board.get(x, y).mine)
            .collect()
    }

    /// The whole point of a flag race: a mine is a prize, not an ending.
    #[test]
    fn a_mine_claims_instead_of_killing() {
        let mut g = Game::new(7, 9, 9, 10, Mode::FlagRace);
        g.apply(&Event::Reveal {
            player: 0,
            x: 4,
            y: 4,
        });
        let mine = mines_of(&g)[0];

        g.apply(&Event::Reveal {
            player: 1,
            x: mine.0,
            y: mine.1,
        });
        assert_eq!(g.board.get(mine.0, mine.1).state, Reveal::Flagged);
        assert_eq!(g.status(), Status::Playing, "a claim must not end the game");

        // The same click in co-op is death, which is the difference.
        let mut c = Game::new(7, 9, 9, 10, Mode::Coop);
        c.apply(&Event::Reveal {
            player: 0,
            x: 4,
            y: 4,
        });
        c.apply(&Event::Reveal {
            player: 1,
            x: mine.0,
            y: mine.1,
        });
        assert_eq!(c.status(), Status::Lost);
    }

    #[test]
    fn a_claimed_mine_cannot_be_handed_back() {
        let mut g = Game::new(7, 9, 9, 10, Mode::FlagRace);
        g.apply(&Event::Reveal {
            player: 0,
            x: 4,
            y: 4,
        });
        let mine = mines_of(&g)[0];
        g.apply(&Event::Reveal {
            player: 0,
            x: mine.0,
            y: mine.1,
        });
        // Claiming again is a no-op rather than a toggle back to hidden.
        g.apply(&Event::Reveal {
            player: 1,
            x: mine.0,
            y: mine.1,
        });
        assert_eq!(g.board.get(mine.0, mine.1).state, Reveal::Flagged);
    }

    #[test]
    fn the_race_ends_when_the_last_mine_is_claimed() {
        let mut g = Game::new(7, 9, 9, 10, Mode::FlagRace);
        g.apply(&Event::Reveal {
            player: 0,
            x: 4,
            y: 4,
        });
        let mines = mines_of(&g);
        for (i, &(x, y)) in mines.iter().enumerate() {
            assert_eq!(g.status(), Status::Playing, "ended early at mine {i}");
            g.apply(&Event::Reveal {
                player: (i % 2) as u8,
                x,
                y,
            });
        }
        assert_eq!(g.status(), Status::Won, "claiming every mine must end it");
    }

    /// Race is co-op's rules on a board each; the engine treats it the same
    /// and the shell folds one log into two boards.
    #[test]
    fn race_keeps_the_ordinary_rules() {
        let mut g = Game::new(7, 9, 9, 10, Mode::Race);
        g.apply(&Event::Reveal {
            player: 0,
            x: 4,
            y: 4,
        });
        let mine = mines_of(&g)[0];
        g.apply(&Event::Reveal {
            player: 0,
            x: mine.0,
            y: mine.1,
        });
        assert_eq!(g.status(), Status::Lost);
    }
}

#[cfg(test)]
mod race_layout_tests {
    use super::*;

    fn layout(first: (u8, u8), mode: Mode) -> Vec<bool> {
        let mut g = Game::new(42, 9, 9, 10, mode);
        g.apply(&Event::Reveal {
            player: 0,
            x: first.0,
            y: first.1,
        });
        g.board.cells().iter().map(|c| c.mine).collect()
    }

    /// The two racers each open somewhere different, and must still be playing
    /// the same board — otherwise it is two solitaire games with one clock.
    #[test]
    fn a_race_deals_both_players_the_same_mines() {
        assert_eq!(
            layout((0, 0), Mode::Race),
            layout((8, 8), Mode::Race),
            "the race layout moved with the first click"
        );
        // Co-op keeps first-click safety, so its layout does depend on it.
        assert_ne!(layout((0, 0), Mode::Coop), layout((8, 8), Mode::Coop));
    }

    #[test]
    fn the_centre_is_the_safe_opening_in_a_race() {
        let mut g = Game::new(42, 9, 9, 10, Mode::Race);
        g.apply(&Event::Reveal {
            player: 0,
            x: 4,
            y: 4,
        });
        assert!(!g.board.get(4, 4).mine, "the agreed opening was a mine");
        assert_eq!(g.status(), Status::Playing);
    }
}
