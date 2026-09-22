use rand::{Rng, RngCore, SeedableRng};
use rand_chacha::ChaCha8Rng;

/// Deterministic RNG for a single Room.
///
/// All gameplay randomness (spawn positions, AI direction picks, point/booster
/// placement) must go through this. Wall-clock RNG (`rand::rng()`) is forbidden
/// inside gameplay code so that bot replays with a fixed `room_seed` reproduce
/// identical sequences for acceptance tests.
#[derive(Debug)]
pub struct RoomRng {
    inner: ChaCha8Rng,
    #[allow(dead_code)]
    pub seed: u64,
}

impl RoomRng {
    pub fn from_seed(seed: u64) -> Self {
        Self { inner: ChaCha8Rng::seed_from_u64(seed), seed }
    }

    pub fn from_entropy() -> Self {
        let mut bytes = [0u8; 8];
        rand::rng().fill_bytes(&mut bytes);
        let seed = u64::from_le_bytes(bytes);
        Self::from_seed(seed)
    }

    pub fn range_usize(&mut self, range: std::ops::Range<usize>) -> usize {
        self.inner.random_range(range)
    }

    pub fn range_f32(&mut self, range: std::ops::Range<f32>) -> f32 {
        self.inner.random_range(range)
    }

    #[allow(dead_code)]
    pub fn next_u32(&mut self) -> u32 {
        self.inner.random()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_sequence() {
        let mut a = RoomRng::from_seed(42);
        let mut b = RoomRng::from_seed(42);
        for _ in 0..100 {
            assert_eq!(a.range_usize(0..1000), b.range_usize(0..1000));
            assert_eq!(a.next_u32(), b.next_u32());
        }
    }

    #[test]
    fn different_seeds_diverge() {
        let mut a = RoomRng::from_seed(1);
        let mut b = RoomRng::from_seed(2);
        let mut diff = 0;
        for _ in 0..32 {
            if a.next_u32() != b.next_u32() {
                diff += 1;
            }
        }
        assert!(diff > 20);
    }
}
