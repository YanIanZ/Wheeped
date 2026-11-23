//! Per-world counters and latency percentiles.
//!
//! "Serialization" is the storage lane's preparation of a commit: taking the dirty chunks as a
//! snapshot nothing changes afterwards. "Backend" is the commit itself, object writes through the
//! generation swap. Turning a chunk into bytes happens before this crate sees them and is not
//! measured here.

use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use crate::lock;

/// The last [`LatencyRecorder::CAPACITY`] latency samples, for p50, p95 and p99.
///
/// A fixed ring, so memory is bounded however long the server runs, and the percentiles describe
/// recent behaviour rather than the whole uptime.
#[derive(Default)]
pub struct LatencyRecorder {
    ring: Mutex<Ring>,
}

#[derive(Default)]
struct Ring {
    /// Samples in nanoseconds. It grows to the capacity and is overwritten in place after that.
    nanos: Vec<u64>,
    /// Samples recorded so far.
    count: u64,
}

/// Percentiles in milliseconds over the retained samples; all zero when there are none.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Percentiles {
    /// How many samples the percentiles are taken over.
    pub samples: usize,
    /// The median.
    pub p50: f64,
    /// The 95th percentile.
    pub p95: f64,
    /// The 99th percentile.
    pub p99: f64,
}

impl LatencyRecorder {
    /// The number of samples kept.
    pub const CAPACITY: usize = 1024;

    /// Records one sample, replacing the oldest once the ring is full.
    pub fn record(&self, elapsed: Duration) {
        let nanos = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
        let mut ring = lock(&self.ring);
        let slot = (ring.count % Self::CAPACITY as u64) as usize;
        match ring.nanos.get_mut(slot) {
            Some(oldest) => *oldest = nanos,
            None => ring.nanos.push(nanos),
        }
        ring.count += 1;
    }

    /// The percentiles of the retained samples.
    #[must_use]
    pub fn percentiles(&self) -> Percentiles {
        let mut sorted = lock(&self.ring).nanos.clone();
        if sorted.is_empty() {
            return Percentiles::default();
        }
        sorted.sort_unstable();
        Percentiles {
            samples: sorted.len(),
            p50: nearest_rank(&sorted, 0.50),
            p95: nearest_rank(&sorted, 0.95),
            p99: nearest_rank(&sorted, 0.99),
        }
    }
}

/// The nearest-rank percentile of sorted nanosecond samples, in milliseconds.
fn nearest_rank(sorted: &[u64], quantile: f64) -> f64 {
    let rank = (quantile * sorted.len() as f64).ceil() as usize;
    let at = rank.saturating_sub(1).min(sorted.len().saturating_sub(1));
    sorted
        .get(at)
        .map_or(0.0, |nanos| *nanos as f64 / 1_000_000.0)
}

/// The counters one world keeps.
#[derive(Default)]
pub struct Metrics {
    /// Commit attempts that failed and were tried again.
    pub retries: AtomicU64,
    /// Saves that failed for good, were refused, or were dropped without running.
    pub failures: AtomicU64,
    /// Bytes read from the store or the base.
    pub bytes_read: AtomicU64,
    /// Bytes the store reported as written by commits.
    pub bytes_written: AtomicU64,
    /// Chunks read through from the base.
    pub materialized: AtomicU64,
    /// Time spent preparing a commit.
    pub serialization: LatencyRecorder,
    /// Time spent in the store's commit.
    pub backend: LatencyRecorder,
}

/// A point-in-time copy of one world's metrics.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MetricsSnapshot {
    /// Chunks held in memory.
    pub resident_chunks: u64,
    /// Chunks written or deleted since they were last committed.
    pub dirty_chunks: u64,
    /// Saves queued or running.
    pub save_queue_depth: u64,
    /// Milliseconds since the oldest pending save was queued, 0 when none is.
    pub oldest_pending_save_millis: u64,
    /// Milliseconds since the oldest dirty chunk became dirty: its first write since it was last
    /// clean, or the snapshot of a commit that carried its previous bytes while it was written
    /// again. Truncated, so it is 0 when no chunk is dirty and also while the oldest one is
    /// younger than a millisecond; [`Self::dirty_chunks`] tells the two apart.
    pub oldest_dirty_millis: u64,
    /// Percentiles of the time spent preparing commits.
    pub serialization: Percentiles,
    /// Percentiles of the time spent in the store's commit.
    pub backend: Percentiles,
    /// Commit attempts that failed and were tried again.
    pub retries: u64,
    /// Saves that failed for good, were refused, or were dropped without running.
    pub failures: u64,
    /// Bytes read from the store or the base.
    pub bytes_read: u64,
    /// Bytes the store reported as written by commits.
    pub bytes_written: u64,
    /// Chunks read through from the base.
    pub materialized: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ported from `AwfWorldTest.latencyPercentilesAreNearestRank`.
    #[test]
    fn latency_percentiles_are_nearest_rank() {
        let recorder = LatencyRecorder::default();
        for millis in 1..=100 {
            recorder.record(Duration::from_millis(millis));
        }
        let percentiles = recorder.percentiles();
        assert_eq!(percentiles.samples, 100);
        assert_eq!(percentiles.p50, 50.0);
        assert_eq!(percentiles.p95, 95.0);
        assert_eq!(percentiles.p99, 99.0);
        for _ in 0..5000 {
            recorder.record(Duration::from_nanos(1));
        }
        assert_eq!(
            recorder.percentiles().samples,
            LatencyRecorder::CAPACITY,
            "bounded"
        );
    }

    #[test]
    fn no_samples_give_zero_percentiles() {
        assert_eq!(
            LatencyRecorder::default().percentiles(),
            Percentiles::default()
        );
    }

    #[test]
    fn a_full_ring_keeps_only_the_newest_samples() {
        let recorder = LatencyRecorder::default();
        for _ in 0..LatencyRecorder::CAPACITY {
            recorder.record(Duration::from_millis(900));
        }
        for _ in 0..LatencyRecorder::CAPACITY {
            recorder.record(Duration::from_millis(2));
        }
        let percentiles = recorder.percentiles();
        assert_eq!(percentiles.samples, LatencyRecorder::CAPACITY);
        assert_eq!(percentiles.p99, 2.0, "every older sample was overwritten");
    }
}
