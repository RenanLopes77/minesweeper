use crate::rng::Rng;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Reveal {
    Hidden = 0,
    Flagged = 1,
    Shown = 2,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Playing,
    Won,
    Lost,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cell {
    pub mine: bool,
    pub adj: u8,
    pub state: Reveal,
}

pub struct Board {
    pub w: u8,
    pub h: u8,
    pub mines: u16,
    cells: Vec<Cell>,
}

impl Board {
    pub fn new(w: u8, h: u8, mines: u16) -> Self {
        let blank = Cell {
            mine: false,
            adj: 0,
            state: Reveal::Hidden,
        };
        Board {
            w,
            h,
            mines,
            cells: vec![blank; w as usize * h as usize],
        }
    }

    pub fn cells(&self) -> &[Cell] {
        &self.cells
    }

    pub fn idx(&self, x: u8, y: u8) -> usize {
        y as usize * self.w as usize + x as usize
    }

    pub fn get(&self, x: u8, y: u8) -> Cell {
        self.cells[self.idx(x, y)]
    }

    pub fn in_bounds(&self, x: i32, y: i32) -> bool {
        x >= 0 && y >= 0 && x < self.w as i32 && y < self.h as i32
    }

    pub fn neighbors(&self, x: u8, y: u8) -> impl Iterator<Item = (u8, u8)> {
        let (w, h) = (self.w as i32, self.h as i32);
        let (x, y) = (x as i32, y as i32);
        [
            (-1, -1),
            (0, -1),
            (1, -1),
            (-1, 0),
            (1, 0),
            (-1, 1),
            (0, 1),
            (1, 1),
        ]
        .into_iter()
        .map(move |(dx, dy)| (x + dx, y + dy))
        .filter(move |&(nx, ny)| nx >= 0 && ny >= 0 && nx < w && ny < h)
        .map(|(nx, ny)| (nx as u8, ny as u8))
    }

    /// Deterministic given `(seed, first_click)`. Both are in the event log,
    /// so every peer reconstructs this exact layout without being told it.
    pub fn place_mines(&mut self, seed: u64, first_click: (u8, u8)) {
        let (sx, sy) = first_click;
        let safe: Vec<usize> = self
            .neighbors(sx, sy)
            .chain(core::iter::once((sx, sy)))
            .map(|(x, y)| self.idx(x, y))
            .collect();

        let mut pool: Vec<usize> = (0..self.cells.len())
            .filter(|i| !safe.contains(i))
            .collect();

        // Total, not asserted: `mines` arrives from a peer, and a board that
        // cannot hold them is a message to survive rather than a bug to trap on.
        let k = (self.mines as usize).min(pool.len());

        // Partial Fisher–Yates: shuffle exactly the first k slots, take those.
        let mut rng = Rng::new(seed);
        for i in 0..k {
            let j = i + rng.below((pool.len() - i) as u64) as usize;
            pool.swap(i, j);
            self.cells[pool[i]].mine = true;
        }

        self.count_adjacent();
    }

    fn count_adjacent(&mut self) {
        for y in 0..self.h {
            for x in 0..self.w {
                if self.get(x, y).mine {
                    continue;
                }
                let n = self
                    .neighbors(x, y)
                    .filter(|&(nx, ny)| self.get(nx, ny).mine)
                    .count();
                let i = self.idx(x, y);
                self.cells[i].adj = n as u8;
            }
        }
    }

    pub fn reveal(&mut self, x: u8, y: u8) {
        let mut stack = vec![(x, y)];
        while let Some((cx, cy)) = stack.pop() {
            let i = self.idx(cx, cy);
            // Hidden is the only openable state: this single check makes
            // flags act as walls and makes re-visits free.
            if self.cells[i].state != Reveal::Hidden {
                continue;
            }
            self.cells[i].state = Reveal::Shown;

            if self.cells[i].adj == 0 && !self.cells[i].mine {
                stack.extend(self.neighbors(cx, cy));
            }
        }
    }

    /// Derived, never stored. A stored copy is a cache, and every mutation
    /// would have to remember to refresh it.
    ///
    /// Note what wins: flags are irrelevant. Flagging every mine does not
    /// win the game — clearing every non-mine cell does.
    pub fn status(&self) -> Status {
        if self
            .cells
            .iter()
            .any(|c| c.mine && c.state == Reveal::Shown)
        {
            return Status::Lost;
        }
        if self
            .cells
            .iter()
            .all(|c| c.mine || c.state == Reveal::Shown)
        {
            return Status::Won;
        }
        Status::Playing
    }

    pub fn toggle_flag(&mut self, x: u8, y: u8) {
        let i = self.idx(x, y);
        self.cells[i].state = match self.cells[i].state {
            Reveal::Hidden => Reveal::Flagged,
            Reveal::Flagged => Reveal::Hidden,
            Reveal::Shown => Reveal::Shown, // already open — ignore
        };
    }

    #[cfg(test)]
    fn cells_mut(&mut self) -> &mut [Cell] {
        &mut self.cells
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn new_board_is_blank() {
        let b = Board::new(9, 9, 10);
        assert_eq!(b.cells().len(), 81);
        assert!(b.cells().iter().all(|c| c.state == Reveal::Hidden));
        assert!(b.cells().iter().all(|c| !c.mine && c.adj == 0));
    }

    #[test]
    fn indexing_is_row_major_and_totalo() {
        let b = Board::new(4, 3, 1);
        assert_eq!(b.idx(0, 0), 0);
        assert_eq!(b.idx(3, 0), 3);
        assert_eq!(b.idx(0, 1), 4);
        assert_eq!(b.idx(3, 2), 11);
    }

    #[test]
    fn bounds_reject_negatives_and_overflow() {
        let b = Board::new(4, 3, 1);
        assert!(b.in_bounds(0, 0));
        assert!(b.in_bounds(3, 2));
        assert!(!b.in_bounds(-1, 0));
        assert!(!b.in_bounds(4, 0));
        assert!(!b.in_bounds(0, 3));
    }

    #[test]
    fn placement_is_deterministic() {
        let mut a = Board::new(16, 16, 40);
        let mut b = Board::new(16, 16, 40);
        a.place_mines(0xC0FFEE, (3, 3));
        b.place_mines(0xC0FFEE, (3, 3));
        let mines_of = |bd: &Board| bd.cells().iter().map(|c| c.mine).collect::<Vec<_>>();
        assert_eq!(mines_of(&a), mines_of(&b));
    }

    #[test]
    fn placement_respects_count_and_safe_zone() {
        let mut b = Board::new(16, 16, 40);
        b.place_mines(1, (5, 5));
        assert_eq!(b.cells().iter().filter(|c| c.mine).count(), 40);
        for dy in -1..=1i32 {
            for dx in -1..=1i32 {
                let (x, y) = ((5 + dx) as u8, (5 + dy) as u8);
                assert!(!b.get(x, y).mine, "mine inside safe zone at {x},{y}");
            }
        }
    }

    #[test]
    fn different_first_click_gives_different_board() {
        let mut a = Board::new(16, 16, 40);
        let mut b = Board::new(16, 16, 40);
        a.place_mines(99, (2, 2));
        b.place_mines(99, (12, 12));
        assert_ne!(
            a.cells().iter().map(|c| c.mine).collect::<Vec<_>>(),
            b.cells().iter().map(|c| c.mine).collect::<Vec<_>>()
        );
    }

    /// A wall of mines down column 2, splitting the board in two:
    ///
    ///     . 2 * ? ?      `.` zero adjacent    `*` mine
    ///     . 3 * ? ?      `2` `3` numbered     `?` unreachable from the left
    ///     . 3 * ? ?
    ///     . 3 * ? ?      click (0,0) and the left half opens; the right
    ///     . 2 * ? ?      half can never be reached through the wall.
    fn fixture() -> Board {
        let mut b = Board::new(5, 5, 5);
        for y in 0..5u8 {
            let i = b.idx(2, y);
            b.cells_mut()[i].mine = true;
        }
        b.count_adjacent();
        b
    }

    /// Prints the board as ASCII. Not a test — a debugging tool. Call it from
    /// any test with `--nocapture` when an assertion says something surprising.
    #[allow(dead_code)]
    fn dump(b: &Board, label: &str) {
        println!("--- {label} ---");
        for y in 0..b.h {
            let mut row = String::new();
            for x in 0..b.w {
                let c = b.get(x, y);
                row.push(match (c.mine, c.state) {
                    (true, Reveal::Shown) => '@',
                    (true, _) => '*',
                    (_, Reveal::Flagged) => 'F',
                    (_, Reveal::Shown) if c.adj == 0 => '.',
                    (_, Reveal::Shown) => (b'0' + c.adj) as char,
                    _ => '#',
                });
                row.push(' ');
            }
            println!("{row}");
        }
    }

    #[test]
    fn flood_opens_region_and_stops_at_numbers() {
        let mut b = fixture();
        b.reveal(0, 0);
        assert_eq!(b.get(0, 0).state, Reveal::Shown);
        assert_eq!(b.get(1, 0).state, Reveal::Shown, "numbered border opens");
        assert_eq!(b.get(2, 0).state, Reveal::Hidden, "mines never auto-open");
        assert_eq!(
            b.get(3, 0).state,
            Reveal::Hidden,
            "past the wall stays shut"
        );
    }

    #[test]
    fn flood_never_crosses_a_flag() {
        let mut b = fixture();
        b.toggle_flag(1, 2);
        b.reveal(0, 2);
        assert_eq!(
            b.get(1, 2).state,
            Reveal::Flagged,
            "flag survives the flood"
        );
        assert_eq!(b.get(0, 2).state, Reveal::Shown);
    }

    #[test]
    fn revealing_a_number_opens_exactly_one_cell() {
        let mut b = fixture();
        b.reveal(1, 0);
        let shown = b
            .cells()
            .iter()
            .filter(|c| c.state == Reveal::Shown)
            .count();
        assert_eq!(shown, 1);
    }

    #[test]
    fn flagging_every_mine_does_not_win() {
        let mut b = fixture();
        for y in 0..5u8 {
            b.toggle_flag(2, y); // every mine on the board, correctly flagged
        }
        assert_eq!(b.status(), Status::Playing);
    }

    #[test]
    fn opening_a_mine_loses_immediately() {
        let mut b = fixture();
        b.reveal(2, 0);
        assert_eq!(b.status(), Status::Lost);
    }

    #[test]
    fn clearing_every_safe_cell_wins() {
        let mut b = fixture();
        for y in 0..5u8 {
            for x in 0..5u8 {
                if !b.get(x, y).mine {
                    b.reveal(x, y);
                }
            }
        }
        assert_eq!(b.status(), Status::Won);
    }

    proptest! {
        #[test]
        fn brute_force_clear_always_wins(seed: u64) {
            let mut b = Board::new(9, 9, 10);
            b.place_mines(seed, (4, 4));
            for y in 0..9u8 {
                for x in 0..9u8 {
                    if !b.get(x, y).mine { b.reveal(x, y); }
                }
            }
            prop_assert_eq!(b.status(), Status::Won);
        }

        #[test]
        fn neighbor_count_is_3_5_or_8(x in 0u8..12, y in 0u8..12) {
            let b = Board::new(12, 12, 0);
            let n = b.neighbors(x, y).count();
            let corner = (x == 0 || x == 11) && (y == 0 || y == 11);
            let edge = x == 0 || x == 11 || y == 0 || y == 11;
            prop_assert_eq!(n, if corner { 3 } else if edge { 5 } else { 8 });
        }

        #[test]
        fn adj_matches_independent_brute_force(seed: u64, sx in 0u8..12, sy in 0u8..12) {
            let mut b = Board::new(12, 12, 20);
            b.place_mines(seed, (sx, sy));

            for y in 0..12u8 {
                for x in 0..12u8 {
                    if b.get(x, y).mine { continue; }

                    // Deliberately does NOT use neighbors() — a test that reuses the
                    // code under test only proves the code agrees with itself.
                    let mut brute = 0u8;
                    for yy in 0..12u8 {
                        for xx in 0..12u8 {
                            let touching = (xx as i32 - x as i32).abs() <= 1
                                && (yy as i32 - y as i32).abs() <= 1
                                && !(xx == x && yy == y);
                            if touching && b.get(xx, yy).mine {
                                brute += 1;
                            }
                        }
                    }
                    prop_assert_eq!(b.get(x, y).adj, brute, "at ({}, {})", x, y);
                }
            }
        }

        #[test]
        fn first_click_never_hits_a_mine(seed: u64, sx in 0u8..12, sy in 0u8..12) {
            let mut b = Board::new(12, 12, 20);
            b.place_mines(seed, (sx, sy));
            b.reveal(sx, sy);
            let opened_mine = b.cells().iter().any(|c| c.mine && c.state == Reveal::Shown);
            prop_assert!(!opened_mine);
        }

        #[test]
        fn flood_terminates_and_stays_in_bounds(seed: u64) {
            let mut b = Board::new(30, 16, 99);   // expert: dense, big open pockets
            b.place_mines(seed, (0, 0));
            b.reveal(0, 0);                        // if this hangs or panics, the test fails
            prop_assert!(b.cells().iter().any(|c| c.state == Reveal::Shown));
        }
    }
}
