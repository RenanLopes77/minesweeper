use crate::{Board, Event, Mode, Reveal, Stamped, Status};

/// FNV-1a. One home for the two constants: the board hash and the log hash
/// are compared between peers, so they must never drift apart.
pub fn fnv1a(bytes: impl IntoIterator<Item = u8>) -> u64 {
    bytes.into_iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01B3)
    })
}

/// The mode the current board is played under: whatever the last `Start` said.
/// It is in the log, so both peers read the same answer.
pub fn mode_of(log: &[Stamped]) -> Mode {
    log.iter()
        .rev()
        .find_map(|s| match s.ev {
            Event::Start { mode, .. } => Some(mode),
            _ => None,
        })
        .unwrap_or_default()
}

/// A race is one log folded into two boards: each player's moves land only on
/// their own copy, and `Start` lands on both because it is the shared deal.
///
/// Returns my board, theirs, whoever finished first, and which event settled
/// it — all in one pass, because "who won" is a question about order.
pub fn race_fold(events: &[Event], me: u8) -> (Game, Game, Option<u8>, Option<usize>) {
    let blank = || match events.first() {
        Some(&Event::Start {
            seed,
            w,
            h,
            mines,
            mode,
        }) => Game::new(seed, w, h, mines, mode),
        _ => Game::new(0, 9, 9, 10, Mode::Race),
    };
    let (mut mine, mut theirs) = (blank(), blank());
    let (mut winner, mut ended) = (None, None);

    for (i, ev) in events.iter().enumerate().skip(1) {
        match *ev {
            Event::Start { .. } => {
                mine.apply(ev);
                theirs.apply(ev);
                winner = None;
                ended = None;
            }
            Event::Reveal { player, .. } | Event::Flag { player, .. } => {
                let board = if player == me { &mut mine } else { &mut theirs };
                board.apply(ev);
                if winner.is_none() {
                    // First board home wins; stepping on a mine hands it over.
                    if mine.status() == Status::Won || theirs.status() == Status::Lost {
                        winner = Some(me);
                    } else if theirs.status() == Status::Won || mine.status() == Status::Lost {
                        winner = Some(1 - me);
                    }
                    if winner.is_some() {
                        ended = Some(i);
                    }
                }
            }
        }
    }
    (mine, theirs, winner, ended)
}

/// The log both peers hold, hashed. In a race their boards are meant to
/// differ, so the agreement worth checking is the log itself.
pub fn log_hash(log: &[Stamped]) -> u64 {
    fnv1a(crate::encode_log(log))
}

pub struct Game {
    pub board: Board,
    pub mode: Mode,
    /// Mines claimed per player in a flag race. Counted here, as the claim
    /// lands, because the board deliberately refuses a second claim on the
    /// same mine — anything that recounts afterwards from the log cannot tell
    /// the winner from the latecomer.
    claims: [u32; 2],
    seed: u64,
    placed: bool,
}

impl Game {
    pub fn new(seed: u64, w: u8, h: u8, mines: u16, mode: Mode) -> Self {
        Game {
            board: Board::new(w, h, mines),
            mode,
            claims: [0; 2],
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
        //
        // It has to be *this* mode's idea of finished: the board calls itself
        // won once every safe cell is open, which in a flag race happens while
        // mines are still unclaimed — and would freeze the game one prize short.
        if self.status() != Status::Playing {
            return;
        }
        match *ev {
            Event::Start { .. } => unreachable!("handled above"),
            Event::Reveal { player, x, y } => {
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
                    self.claim_for(player, x, y);
                    return;
                }
                self.board.reveal(x, y);
            }
            Event::Flag { player, x, y } => {
                if !self.board.in_bounds(x as i32, y as i32) {
                    return;
                }
                // In a flag race a flag on a mine *is* a claim — the same move
                // by another name. Letting it flag without crediting anyone
                // ends the race with a mine nobody owns and a scoreboard that
                // does not add up to the mines on the table.
                if self.mode == Mode::FlagRace && self.board.get(x, y).mine {
                    self.claim_for(player, x, y);
                    return;
                }
                self.board.toggle_flag(x, y);
            }
        }
    }

    /// Takes a mine for a player, if it is still there to take. A mine that
    /// has already been claimed stays with whoever got there first.
    fn claim_for(&mut self, player: u8, x: u8, y: u8) {
        if self.board.claim(x, y) {
            if let Some(slot) = self.claims.get_mut(player as usize) {
                *slot += 1;
            }
        }
    }

    /// A flag race ends when the last mine is claimed — nobody can lose it,
    /// so the board's own "did anyone step on a mine" rule never fires and
    /// its "is everything safe uncovered" rule would drag the game past the
    /// point where the result is settled.
    pub fn status(&self) -> Status {
        if self.mode == Mode::FlagRace {
            // Before the first click there are no mines, so "every mine is
            // claimed" is vacuously true — and would end the game at deal.
            if !self.placed {
                return Status::Playing;
            }
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

    /// Mines claimed by each player in a flag race, in player order. Zero for
    /// every other mode, where mines are not a prize.
    pub fn scores(&self) -> [u32; 2] {
        self.claims
    }

    /// Replays, and says which event ended the game.
    ///
    /// The last event in a log is not the one that ended it: peers keep
    /// sending moves until they hear the bad news, and those arrive, merge,
    /// and are then ignored by `apply`. Anything reading the log for a
    /// finishing time has to know where the game actually stopped.
    pub fn replay_marking_end(events: &[Event]) -> Option<(Game, Option<usize>)> {
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
        let mut ended = None;
        for (i, ev) in events.iter().enumerate().skip(1) {
            g.apply(ev);
            match (ended, g.status()) {
                (None, Status::Playing) => {}
                // A restart puts the game back in play, so the ending it had
                // before is no longer the ending.
                (Some(_), Status::Playing) => ended = None,
                (None, _) => ended = Some(i),
                (Some(_), _) => {}
            }
        }
        Some((g, ended))
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

    /// FNV-1a over the whole game. Two peers compare this after each event and
    /// detect divergence in 8 bytes instead of shipping the whole board.
    ///
    /// It covers the *deal* as well as the cells — the shape of the board, the
    /// mine count, the mode and the scoreboard. Hashing cells alone let two
    /// peers agree while playing different games: a blank 9x9/10 co-op board
    /// and a blank 9x9/40 race board are identical cell for cell, and a flag
    /// race's claims are state that no cell records.
    ///
    /// Bit budget per cell: mine 1, adj 4 (0..=8), state 2 (0..=2) = 7.
    pub fn hash(&self) -> u64 {
        let mut bytes: Vec<u8> = Vec::with_capacity(self.board.cells().len() + 16);
        let mut mix = |byte: u8| bytes.push(byte);

        mix(self.board.w);
        mix(self.board.h);
        for byte in self.board.mines.to_le_bytes() {
            mix(byte);
        }
        mix(self.mode as u8);
        for score in self.claims {
            for byte in score.to_le_bytes() {
                mix(byte);
            }
        }
        for c in self.board.cells() {
            mix((c.mine as u8) | (c.adj << 1) | ((c.state as u8) << 5));
        }
        fnv1a(bytes)
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

#[cfg(test)]
mod flag_race_endgame {
    use super::*;

    /// Clearing every safe cell is "won" to a plain board, but a flag race is
    /// not over until the mines are claimed — the guard on incoming moves has
    /// to agree, or the last mine can never be taken.
    #[test]
    fn the_last_mine_is_still_claimable_once_the_board_is_clear() {
        let mut g = Game::new(11, 9, 9, 10, Mode::FlagRace);
        g.apply(&Event::Reveal {
            player: 0,
            x: 4,
            y: 4,
        });

        let cells: Vec<(u8, u8)> = (0..9u8)
            .flat_map(|y| (0..9u8).map(move |x| (x, y)))
            .collect();
        // Deciding which cells to click has to finish before clicking starts:
        // the filter borrows the board that `apply` wants to mutate.
        type Coords = Vec<(u8, u8)>;
        let (mines, safe): (Coords, Coords) =
            cells.iter().partition(|&&(x, y)| g.board.get(x, y).mine);
        // Every safe cell first: this is what makes the board call itself won.
        for (x, y) in safe {
            g.apply(&Event::Reveal { player: 0, x, y });
        }
        for (i, &(x, y)) in mines.iter().enumerate() {
            g.apply(&Event::Reveal {
                player: (i % 2) as u8,
                x,
                y,
            });
        }

        let claimed = g
            .board
            .cells()
            .iter()
            .filter(|c| c.mine && c.state == Reveal::Flagged)
            .count();
        assert_eq!(claimed, 10, "a mine was left unclaimable");
        assert_eq!(g.status(), Status::Won);
    }
}

#[cfg(test)]
mod claim_ownership {
    use super::*;

    fn opened(seed: u64) -> (Game, Vec<(u8, u8)>) {
        let mut g = Game::new(seed, 9, 9, 10, Mode::FlagRace);
        g.apply(&Event::Reveal {
            player: 0,
            x: 4,
            y: 4,
        });
        let mines = (0..9u8)
            .flat_map(|y| (0..9u8).map(move |x| (x, y)))
            .filter(|&(x, y)| g.board.get(x, y).mine)
            .collect();
        (g, mines)
    }

    /// The board refuses a second claim on the same mine. The score has to
    /// refuse it too, or clicking a mine your opponent already took quietly
    /// moves the point across the table.
    #[test]
    fn a_re_click_does_not_steal_a_claim() {
        let (mut g, mines) = opened(7);
        let (x, y) = mines[0];

        g.apply(&Event::Reveal { player: 0, x, y });
        assert_eq!(g.scores(), [1, 0]);

        g.apply(&Event::Reveal { player: 1, x, y });
        assert_eq!(g.scores(), [1, 0], "the second click took the point");
        assert_eq!(g.board.get(x, y).state, Reveal::Flagged);
    }

    #[test]
    fn each_claim_credits_the_player_who_landed_it() {
        let (mut g, mines) = opened(7);
        for (i, &(x, y)) in mines.iter().enumerate() {
            g.apply(&Event::Reveal {
                player: (i % 2) as u8,
                x,
                y,
            });
        }
        assert_eq!(g.scores(), [5, 5]);
        assert_eq!(g.status(), Status::Won);
    }

    #[test]
    fn a_restart_wipes_the_scoreboard() {
        let (mut g, mines) = opened(7);
        g.apply(&Event::Reveal {
            player: 0,
            x: mines[0].0,
            y: mines[0].1,
        });
        assert_eq!(g.scores(), [1, 0]);

        g.apply(&Event::Start {
            seed: 8,
            w: 9,
            h: 9,
            mines: 10,
            mode: Mode::FlagRace,
        });
        assert_eq!(g.scores(), [0, 0]);
    }

    /// A peer can put any byte in `player`. Indexing the scoreboard with it
    /// must not panic, and the claim still lands on the board.
    #[test]
    fn a_claim_from_an_unknown_player_is_not_fatal() {
        let (mut g, mines) = opened(7);
        let (x, y) = mines[0];
        g.apply(&Event::Reveal { player: 200, x, y });
        assert_eq!(g.scores(), [0, 0]);
        assert_eq!(g.board.get(x, y).state, Reveal::Flagged);
    }

    /// Only mines are prizes: an ordinary flag on a safe cell scores nothing.
    #[test]
    fn flagging_a_safe_cell_scores_nothing() {
        let (mut g, mines) = opened(7);
        let safe = (0..9u8)
            .flat_map(|y| (0..9u8).map(move |x| (x, y)))
            .find(|c| !mines.contains(c))
            .unwrap();
        g.apply(&Event::Flag {
            player: 0,
            x: safe.0,
            y: safe.1,
        });
        assert_eq!(g.scores(), [0, 0]);
    }
}

#[cfg(test)]
mod hostile {
    use super::*;
    use proptest::prelude::*;

    /// A degenerate board is not a win. `all()` over no cells is true, so an
    /// empty board used to report `Won` before anyone had clicked, freezing
    /// the game — `apply` refuses moves once the status leaves `Playing`.
    #[test]
    fn a_flag_race_is_not_won_before_the_first_click() {
        let g = Game::new(1, 9, 9, 10, Mode::FlagRace);
        assert_eq!(g.status(), Status::Playing);
        let none = Game::new(1, 9, 9, 0, Mode::FlagRace);
        assert_eq!(
            none.status(),
            Status::Playing,
            "a mineless deal ended at once"
        );
    }

    #[test]
    fn a_claimed_mine_cannot_be_unflagged() {
        let mut g = Game::new(7, 9, 9, 10, Mode::FlagRace);
        g.apply(&Event::Reveal {
            player: 0,
            x: 4,
            y: 4,
        });
        let (x, y) = (0..9u8)
            .flat_map(|y| (0..9u8).map(move |x| (x, y)))
            .find(|&(x, y)| g.board.get(x, y).mine)
            .unwrap();

        g.apply(&Event::Reveal { player: 0, x, y });
        g.apply(&Event::Flag { player: 1, x, y });
        assert_eq!(
            g.board.get(x, y).state,
            Reveal::Flagged,
            "a flag click gave away someone's claim"
        );
        assert_eq!(g.scores(), [1, 0]);
    }

    proptest! {
        /// The trust boundary, end to end: whatever a peer puts in a Start,
        /// folding it must not panic. `place_mines` used to assert when the
        /// mines could not fit, which aborts the whole wasm module.
        #[test]
        fn any_start_a_peer_can_send_folds_without_panicking(
            seed in any::<u64>(),
            w in 0..=40u8,
            h in 0..=40u8,
            mines in any::<u16>(),
            x in any::<u8>(),
            y in any::<u8>(),
            mode in prop_oneof![Just(Mode::Coop), Just(Mode::FlagRace), Just(Mode::Race)],
        ) {
            let start = Event::Start { seed, w, h, mines, mode };
            let mut g = Game::new(seed, w, h, mines, mode);
            g.apply(&start);
            g.apply(&Event::Reveal { player: 0, x, y });
            g.apply(&Event::Flag { player: 1, x, y });
            let _ = g.status();
            let _ = g.hash();
            let _ = g.scores();
        }

        /// Placement is total: never more mines than there are cells to hold
        /// them, and never a panic reaching for the pool.
        #[test]
        fn placement_never_exceeds_the_board(w in 1..=20u8, h in 1..=20u8, mines in any::<u16>()) {
            let mut b = Board::new(w, h, mines);
            b.place_mines(9, (0, 0));
            let placed = b.cells().iter().filter(|c| c.mine).count();
            prop_assert!(placed <= b.cells().len());
            prop_assert!(placed <= mines as usize);
        }
    }
}

#[cfg(test)]
mod flag_race_bookkeeping {
    use super::*;
    use proptest::prelude::*;

    /// The scoreboard has to account for every mine on the table. A flag on a
    /// mine used to plant a flag without crediting anyone, so the race ended
    /// with a mine nobody owned and a tally that did not add up.
    #[test]
    fn a_right_click_claims_rather_than_just_flagging() {
        let mut g = Game::new(7, 9, 9, 10, Mode::FlagRace);
        g.apply(&Event::Reveal {
            player: 0,
            x: 4,
            y: 4,
        });
        let mines: Vec<(u8, u8)> = (0..9u8)
            .flat_map(|y| (0..9u8).map(move |x| (x, y)))
            .filter(|&(x, y)| g.board.get(x, y).mine)
            .collect();

        g.apply(&Event::Flag {
            player: 1,
            x: mines[0].0,
            y: mines[0].1,
        });
        assert_eq!(g.scores(), [0, 1], "a flagged mine credited nobody");

        for &(x, y) in &mines[1..] {
            g.apply(&Event::Reveal { player: 0, x, y });
        }
        assert_eq!(g.status(), Status::Won);
        assert_eq!(
            g.scores().iter().sum::<u32>() as u16,
            g.board.mines,
            "the race ended without every mine being owned"
        );
    }

    proptest! {
        /// Any deal the wire accepts must be a deal that can be finished: the
        /// safe zone means a board can hold fewer mines than it has cells, and
        /// the ones that go unplaced are prizes a flag race waits for forever.
        #[test]
        fn a_playable_deal_lays_every_mine_it_promises(
            (w, h, mines) in (3..=Event::MAX_W, 3..=Event::MAX_H).prop_flat_map(|(w, h)| {
                let room = w as u16 * h as u16 - Event::SAFE_ZONE as u16;
                (Just(w), Just(h), 0..=room)
            }),
            seed in any::<u64>(),
        ) {
            let start = Event::Start { seed, w, h, mines, mode: Mode::FlagRace };
            prop_assert!(start.is_playable(), "the generator produced an illegal deal");

            let mut g = Game::new(seed, w, h, mines, Mode::FlagRace);
            g.apply(&Event::Reveal { player: 0, x: w / 2, y: h / 2 });
            let laid = g.board.cells().iter().filter(|c| c.mine).count();
            prop_assert_eq!(laid, mines as usize, "a legal deal was short of mines");
        }
    }
}

#[cfg(test)]
mod hash_covers_the_deal {
    use super::*;

    /// Cells alone cannot tell two games apart. A blank board looks the same
    /// whatever it was dealt from, so peers could agree on the hash while
    /// playing different mine counts, different sizes, or different rules.
    #[test]
    fn a_different_deal_hashes_differently() {
        let base = Game::new(1, 9, 9, 10, Mode::Coop);
        assert_ne!(
            base.hash(),
            Game::new(1, 9, 9, 40, Mode::Coop).hash(),
            "mines"
        );
        assert_ne!(
            base.hash(),
            Game::new(1, 16, 16, 10, Mode::Coop).hash(),
            "size"
        );
        assert_ne!(
            base.hash(),
            Game::new(1, 9, 9, 10, Mode::Race).hash(),
            "mode"
        );
        // The seed is not in the hash and must not be: it shows up as soon as
        // the mines are placed, and before that two peers dealing the same
        // game from different seeds have genuinely identical state.
        assert_eq!(base.hash(), Game::new(2, 9, 9, 10, Mode::Coop).hash());
    }

    /// A flag race's scoreboard is state no cell records: the same board can
    /// belong to either player, and only the hash can say so.
    #[test]
    fn the_scoreboard_is_part_of_the_state() {
        let deal = |p: u8| {
            let mut g = Game::new(7, 9, 9, 10, Mode::FlagRace);
            g.apply(&Event::Reveal {
                player: 0,
                x: 4,
                y: 4,
            });
            let mine = (0..9u8)
                .flat_map(|y| (0..9u8).map(move |x| (x, y)))
                .find(|&(x, y)| g.board.get(x, y).mine)
                .unwrap();
            g.apply(&Event::Reveal {
                player: p,
                x: mine.0,
                y: mine.1,
            });
            g
        };
        let (red, blue) = (deal(0), deal(1));
        assert_eq!(red.scores(), [1, 0]);
        assert_eq!(blue.scores(), [0, 1]);
        assert_ne!(
            red.hash(),
            blue.hash(),
            "the same board with the point on the other side hashed the same"
        );
    }
}

#[cfg(test)]
mod where_the_game_ended {
    use super::*;

    fn deal() -> Event {
        Event::Start {
            seed: 7,
            w: 9,
            h: 9,
            mines: 10,
            mode: Mode::Coop,
        }
    }

    /// The log keeps growing after the game stops: a peer that has not yet
    /// heard the bad news sends more moves, and they merge in behind the one
    /// that ended it. The finishing time is the losing move, not the last.
    #[test]
    fn the_ending_move_is_named_not_the_last_one() {
        let mut probe = Game::new(7, 9, 9, 10, Mode::Coop);
        probe.apply(&Event::Reveal {
            player: 0,
            x: 4,
            y: 4,
        });
        let mine = (0..9u8)
            .flat_map(|y| (0..9u8).map(move |x| (x, y)))
            .find(|&(x, y)| probe.board.get(x, y).mine)
            .unwrap();

        let log = vec![
            deal(),
            Event::Reveal {
                player: 0,
                x: 4,
                y: 4,
            },
            Event::Reveal {
                player: 0,
                x: mine.0,
                y: mine.1,
            }, // index 2 — this is the end
            Event::Flag {
                player: 1,
                x: 0,
                y: 0,
            }, // a straggler, ignored by the rules
        ];
        let (g, ended) = Game::replay_marking_end(&log).unwrap();
        assert_eq!(g.status(), Status::Lost);
        assert_eq!(ended, Some(2), "the straggler was mistaken for the ending");
    }

    #[test]
    fn a_game_still_running_has_no_ending() {
        let log = vec![
            deal(),
            Event::Reveal {
                player: 0,
                x: 4,
                y: 4,
            },
        ];
        assert_eq!(Game::replay_marking_end(&log).unwrap().1, None);
    }

    /// A restart puts the game back in play, so the ending it had is not one.
    #[test]
    fn a_restart_clears_the_ending() {
        let mut probe = Game::new(7, 9, 9, 10, Mode::Coop);
        probe.apply(&Event::Reveal {
            player: 0,
            x: 4,
            y: 4,
        });
        let mine = (0..9u8)
            .flat_map(|y| (0..9u8).map(move |x| (x, y)))
            .find(|&(x, y)| probe.board.get(x, y).mine)
            .unwrap();

        let log = vec![
            deal(),
            Event::Reveal {
                player: 0,
                x: 4,
                y: 4,
            },
            Event::Reveal {
                player: 0,
                x: mine.0,
                y: mine.1,
            },
            deal(),
        ];
        let (g, ended) = Game::replay_marking_end(&log).unwrap();
        assert_eq!(g.status(), Status::Playing);
        assert_eq!(ended, None);
    }
}
