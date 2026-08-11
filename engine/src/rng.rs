pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        let s = seed ^ 0x9E37_79B9_7F4A_7C15;
        Rng(if s == 0 { 0xDEAD_BEEF_CAFE_F00D } else { s })
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn below(&mut self, n: u64) -> u64 {
        assert!(n > 0, "below 0 has no valid answer");
        let limit = u64::MAX - (u64::MAX % n);
        loop {
            let v = self.next_u64();
            if v < limit {
                return v % n;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_stream() {
        let a: Vec<u64> = (0..8).map(|_| Rng::new(42).next_u64()).collect();
        let mut r = Rng::new(42);
        let b: Vec<u64> = (0..8).map(|_| r.next_u64()).collect();
        assert_eq!(a[0], b[0]);
        assert_ne!(b[0], b[1]);
    }

    #[test]
    fn zero_seed_is_not_a_dead_stream() {
        let mut r = Rng::new(0);
        assert_ne!(r.next_u64(), 0);
        assert_ne!(r.next_u64(), r.next_u64());
    }

    #[test]
    fn below_stays_in_range() {
        let mut r = Rng::new(7);
        for _ in 0..10_000 {
            assert!(r.below(6) < 6);
        }
    }

    #[test]
    fn below_is_roughly_uniform() {
        let mut r = Rng::new(1);
        let mut counts = [0u32; 6];
        for _ in 0..60_000 {
            counts[r.below(6) as usize] += 1;
        }
        assert!(
            counts.iter().all(|&c| (9_500..10_500).contains(&c)),
            "{counts:?}"
        );
    }
}
