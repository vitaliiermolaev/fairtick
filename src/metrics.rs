// Stage 7 — percentile-based telemetry buffer.
//
// Stores the last `WINDOW` samples in a ring and computes p50/p95/p99/max on
// demand. Used by the room loop to track tick_duration; the same primitive
// will be reused for snapshot_encode/room_update/json_decode percentiles when
// Stage 7.5's audit pipeline lands.
//
// Wall-clock IS allowed here — it's at the telemetry/logging boundary, not
// inside gameplay decisions.

use std::time::Duration;

/// Sliding window of the most recent samples in microseconds. Microseconds
/// keep enough resolution for sub-ms tick durations without the overhead of
/// nanos in the percentile sort.
pub struct Percentiles {
    samples: Vec<u64>,
    next: usize,
    filled: bool,
    capacity: usize,
}

impl Percentiles {
    pub fn new(window: usize) -> Self {
        Self { samples: vec![0; window], next: 0, filled: false, capacity: window }
    }

    pub fn push_us(&mut self, micros: u64) {
        self.samples[self.next] = micros;
        self.next = (self.next + 1) % self.capacity;
        if self.next == 0 {
            self.filled = true;
        }
    }

    pub fn push_duration(&mut self, d: Duration) {
        self.push_us(d.as_micros() as u64);
    }

    /// Number of samples currently held (≤ capacity).
    pub fn len(&self) -> usize {
        if self.filled {
            self.capacity
        } else {
            self.next
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn percentile_us(&self, p: f64) -> u64 {
        let n = self.len();
        if n == 0 {
            return 0;
        }
        let mut buf: Vec<u64> = self.samples[..n].to_vec();
        buf.sort_unstable();
        // Nearest-rank scheme indexed from 0: at p=99 with n=100 this returns
        // buf[99] (the lone outlier), at p=50 it returns buf[50]. Matches the
        // intuitive "value below which p% of samples fall" reading.
        let idx = ((p / 100.0) * n as f64) as usize;
        buf[idx.min(n - 1)]
    }

    pub fn p50_ms(&self) -> f64 {
        self.percentile_us(50.0) as f64 / 1000.0
    }
    pub fn p95_ms(&self) -> f64 {
        self.percentile_us(95.0) as f64 / 1000.0
    }
    pub fn p99_ms(&self) -> f64 {
        self.percentile_us(99.0) as f64 / 1000.0
    }

    pub fn max_ms(&self) -> f64 {
        let n = self.len();
        if n == 0 {
            return 0.0;
        }
        (*self.samples[..n].iter().max().unwrap_or(&0)) as f64 / 1000.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_returns_zero() {
        let p = Percentiles::new(100);
        assert_eq!(p.p99_ms(), 0.0);
        assert_eq!(p.max_ms(), 0.0);
    }

    #[test]
    fn percentiles_track_sample_distribution() {
        let mut p = Percentiles::new(100);
        // 99 samples of 10ms, 1 sample of 100ms → p99 == max == 100ms.
        for _ in 0..99 {
            p.push_us(10_000);
        }
        p.push_us(100_000);
        assert_eq!(p.p50_ms(), 10.0);
        assert_eq!(p.p95_ms(), 10.0);
        assert_eq!(p.p99_ms(), 100.0);
        assert_eq!(p.max_ms(), 100.0);
    }

    #[test]
    fn ring_overwrites_oldest_samples() {
        let mut p = Percentiles::new(4);
        for v in [1_000u64, 2_000, 3_000, 4_000, 5_000] {
            p.push_us(v);
        }
        // After 5 pushes into a 4-wide ring, the 1_000 sample is gone.
        assert_eq!(p.len(), 4);
        assert_eq!(p.max_ms(), 5.0);
    }
}
