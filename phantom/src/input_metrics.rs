use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const HISTOGRAM_BUCKETS: usize = 64;
const REPORT_INTERVAL: Duration = Duration::from_secs(5);

pub struct InputMetrics {
    enabled: bool,
    queue_events: AtomicUsize,
    queue_high_water: AtomicUsize,
    syn_dropped: AtomicU64,
    route_count: AtomicU64,
    route_histogram: [AtomicU64; HISTOGRAM_BUCKETS],
    backend_count: AtomicU64,
    backend_total_us: AtomicU64,
    backend_max_us: AtomicU64,
    backend_slow_200ms: AtomicU64,
    relay_count: AtomicU64,
    relay_total_us: AtomicU64,
    relay_max_us: AtomicU64,
    drain_gap_max_us: AtomicU64,
    last_report: std::sync::Mutex<Instant>,
}

impl InputMetrics {
    pub fn from_environment() -> Self {
        let enabled = std::env::var("PHANTOM_INPUT_METRICS")
            .ok()
            .as_deref()
            .is_some_and(|value| matches!(value, "1" | "true" | "TRUE" | "yes" | "YES"));
        Self {
            enabled,
            queue_events: AtomicUsize::new(0),
            queue_high_water: AtomicUsize::new(0),
            syn_dropped: AtomicU64::new(0),
            route_count: AtomicU64::new(0),
            route_histogram: std::array::from_fn(|_| AtomicU64::new(0)),
            backend_count: AtomicU64::new(0),
            backend_total_us: AtomicU64::new(0),
            backend_max_us: AtomicU64::new(0),
            backend_slow_200ms: AtomicU64::new(0),
            relay_count: AtomicU64::new(0),
            relay_total_us: AtomicU64::new(0),
            relay_max_us: AtomicU64::new(0),
            drain_gap_max_us: AtomicU64::new(0),
            last_report: std::sync::Mutex::new(Instant::now()),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn queue_add(&self, events: usize) {
        if self.enabled {
            let depth = self.queue_events.fetch_add(events, Ordering::Relaxed) + events;
            self.queue_high_water.fetch_max(depth, Ordering::Relaxed);
        }
    }

    pub fn queue_remove(&self, events: usize) {
        if self.enabled {
            self.queue_events.fetch_sub(events, Ordering::Relaxed);
        }
    }

    pub fn record_syn_dropped(&self, count: usize) {
        if self.enabled {
            self.syn_dropped.fetch_add(count as u64, Ordering::Relaxed);
        }
    }

    pub fn record_route_latency(&self, latency: Duration) {
        if !self.enabled {
            return;
        }
        self.route_count.fetch_add(1, Ordering::Relaxed);
        let bucket = latency_bucket(latency.as_micros() as u64);
        self.route_histogram[bucket].fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_evdev_to_route_latency(&self, timestamp: (i64, i64)) {
        if !self.enabled {
            return;
        }
        let mut now = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) } != 0 {
            return;
        }
        let event_us = timestamp.0.saturating_mul(1_000_000) + timestamp.1;
        let now_us = now.tv_sec.saturating_mul(1_000_000) + now.tv_nsec / 1_000;
        if now_us >= event_us {
            self.record_route_latency(Duration::from_micros((now_us - event_us) as u64));
        }
    }

    pub fn record_backend(&self, elapsed: Duration) {
        if !self.enabled {
            return;
        }
        let micros = elapsed.as_micros().min(u64::MAX as u128) as u64;
        self.backend_count.fetch_add(1, Ordering::Relaxed);
        self.backend_total_us.fetch_add(micros, Ordering::Relaxed);
        self.backend_max_us.fetch_max(micros, Ordering::Relaxed);
        if elapsed >= Duration::from_millis(200) {
            self.backend_slow_200ms.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn record_relay(&self, elapsed: Duration) {
        if !self.enabled {
            return;
        }
        let micros = elapsed.as_micros().min(u64::MAX as u128) as u64;
        self.relay_count.fetch_add(1, Ordering::Relaxed);
        self.relay_total_us.fetch_add(micros, Ordering::Relaxed);
        self.relay_max_us.fetch_max(micros, Ordering::Relaxed);
    }

    pub fn record_drain_gap(&self, elapsed: Duration) {
        if self.enabled {
            self.drain_gap_max_us.fetch_max(
                elapsed.as_micros().min(u64::MAX as u128) as u64,
                Ordering::Relaxed,
            );
        }
    }

    pub fn maybe_report(&self) {
        if !self.enabled {
            return;
        }
        let Ok(mut last_report) = self.last_report.lock() else {
            return;
        };
        if last_report.elapsed() < REPORT_INTERVAL {
            return;
        }
        *last_report = Instant::now();

        let count = self.route_count.swap(0, Ordering::Relaxed);
        let buckets: [u64; HISTOGRAM_BUCKETS] =
            std::array::from_fn(|idx| self.route_histogram[idx].swap(0, Ordering::Relaxed));
        let backend_calls = self.backend_count.swap(0, Ordering::Relaxed);
        let backend_total_us = self.backend_total_us.swap(0, Ordering::Relaxed);
        let relay_calls = self.relay_count.swap(0, Ordering::Relaxed);
        let relay_total_us = self.relay_total_us.swap(0, Ordering::Relaxed);
        tracing::info!(
            route_events = count,
            route_p50_us = percentile(&buckets, count, 50),
            route_p95_us = percentile(&buckets, count, 95),
            route_p99_us = percentile(&buckets, count, 99),
            queue_depth_events = self.queue_events.load(Ordering::Relaxed),
            queue_high_water_events = self.queue_high_water.swap(0, Ordering::Relaxed),
            syn_dropped = self.syn_dropped.swap(0, Ordering::Relaxed),
            max_epoll_drain_gap_us = self.drain_gap_max_us.swap(0, Ordering::Relaxed),
            backend_calls,
            backend_avg_us = average(backend_total_us, backend_calls),
            backend_max_us = self.backend_max_us.swap(0, Ordering::Relaxed),
            backend_stalls_over_200ms = self.backend_slow_200ms.swap(0, Ordering::Relaxed),
            desktop_relay_calls = relay_calls,
            desktop_relay_avg_us = average(relay_total_us, relay_calls),
            desktop_relay_max_us = self.relay_max_us.swap(0, Ordering::Relaxed),
            "input latency metrics (5s window)"
        );
    }
}

fn average(total_us: u64, count: u64) -> u64 {
    total_us.checked_div(count).unwrap_or_default()
}

fn latency_bucket(micros: u64) -> usize {
    if micros <= 1 {
        0
    } else {
        (64 - micros.leading_zeros() as usize).min(HISTOGRAM_BUCKETS - 1)
    }
}

fn percentile(buckets: &[u64; HISTOGRAM_BUCKETS], count: u64, percentile: u64) -> u64 {
    if count == 0 {
        return 0;
    }
    let target = count.saturating_mul(percentile).div_ceil(100);
    let mut accumulated = 0;
    for (idx, bucket_count) in buckets.iter().enumerate() {
        accumulated += bucket_count;
        if accumulated >= target {
            return if idx == 0 { 1 } else { 1u64 << idx };
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::{latency_bucket, percentile, HISTOGRAM_BUCKETS};

    #[test]
    fn histogram_percentiles_are_monotonic_and_bounded() {
        let mut buckets = [0; HISTOGRAM_BUCKETS];
        for sample in [50, 100, 200, 500, 1_000, 10_000, 500_000] {
            buckets[latency_bucket(sample)] += 1;
        }
        let p50 = percentile(&buckets, 7, 50);
        let p95 = percentile(&buckets, 7, 95);
        let p99 = percentile(&buckets, 7, 99);
        assert!(p50 <= p95 && p95 <= p99);
        assert!(p50 <= 1_000);
        assert!(p99 >= 500_000);
    }
}
