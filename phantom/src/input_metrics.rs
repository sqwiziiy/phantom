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
    queue_wait_count: AtomicU64,
    queue_wait_histogram: [AtomicU64; HISTOGRAM_BUCKETS],
    output_queue_wait_count: AtomicU64,
    output_queue_wait_histogram: [AtomicU64; HISTOGRAM_BUCKETS],
    output_queue_commands: AtomicUsize,
    output_queue_high_water: AtomicUsize,
    evdev_to_send_count: AtomicU64,
    evdev_to_send_histogram: [AtomicU64; HISTOGRAM_BUCKETS],
    evdev_read_count: AtomicU64,
    evdev_read_total_us: AtomicU64,
    evdev_read_max_us: AtomicU64,
    backend_count: AtomicU64,
    backend_total_us: AtomicU64,
    backend_max_us: AtomicU64,
    backend_slow_200ms: AtomicU64,
    android_socket_count: AtomicU64,
    android_socket_total_us: AtomicU64,
    android_socket_max_us: AtomicU64,
    android_socket_slow_200ms: AtomicU64,
    relay_count: AtomicU64,
    relay_total_us: AtomicU64,
    relay_max_us: AtomicU64,
    drain_gap_max_us: AtomicU64,
    last_report: std::sync::Mutex<Instant>,
    last_cpu_us: AtomicU64,
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
            queue_wait_count: AtomicU64::new(0),
            queue_wait_histogram: std::array::from_fn(|_| AtomicU64::new(0)),
            output_queue_wait_count: AtomicU64::new(0),
            output_queue_wait_histogram: std::array::from_fn(|_| AtomicU64::new(0)),
            output_queue_commands: AtomicUsize::new(0),
            output_queue_high_water: AtomicUsize::new(0),
            evdev_to_send_count: AtomicU64::new(0),
            evdev_to_send_histogram: std::array::from_fn(|_| AtomicU64::new(0)),
            evdev_read_count: AtomicU64::new(0),
            evdev_read_total_us: AtomicU64::new(0),
            evdev_read_max_us: AtomicU64::new(0),
            backend_count: AtomicU64::new(0),
            backend_total_us: AtomicU64::new(0),
            backend_max_us: AtomicU64::new(0),
            backend_slow_200ms: AtomicU64::new(0),
            android_socket_count: AtomicU64::new(0),
            android_socket_total_us: AtomicU64::new(0),
            android_socket_max_us: AtomicU64::new(0),
            android_socket_slow_200ms: AtomicU64::new(0),
            relay_count: AtomicU64::new(0),
            relay_total_us: AtomicU64::new(0),
            relay_max_us: AtomicU64::new(0),
            drain_gap_max_us: AtomicU64::new(0),
            last_report: std::sync::Mutex::new(Instant::now()),
            last_cpu_us: AtomicU64::new(process_cpu_us()),
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

    pub fn record_queue_wait(&self, latency: Duration, events: usize) {
        if self.enabled && events > 0 {
            self.queue_wait_count
                .fetch_add(events as u64, Ordering::Relaxed);
            self.queue_wait_histogram[latency_bucket(latency.as_micros() as u64)]
                .fetch_add(events as u64, Ordering::Relaxed);
        }
    }

    pub fn record_output_queue_wait(&self, latency: Duration, commands: usize) {
        if self.enabled && commands > 0 {
            self.output_queue_wait_count
                .fetch_add(commands as u64, Ordering::Relaxed);
            self.output_queue_wait_histogram[latency_bucket(latency.as_micros() as u64)]
                .fetch_add(commands as u64, Ordering::Relaxed);
        }
    }

    pub fn output_queue_add(&self, commands: usize) {
        if self.enabled {
            let depth = self
                .output_queue_commands
                .fetch_add(commands, Ordering::Relaxed)
                + commands;
            self.output_queue_high_water
                .fetch_max(depth, Ordering::Relaxed);
        }
    }

    pub fn output_queue_remove(&self, commands: usize) {
        if self.enabled {
            self.output_queue_commands
                .fetch_sub(commands, Ordering::Relaxed);
        }
    }

    pub fn record_evdev_to_send(&self, latency: Duration) {
        if self.enabled {
            self.evdev_to_send_count.fetch_add(1, Ordering::Relaxed);
            self.evdev_to_send_histogram[latency_bucket(latency.as_micros() as u64)]
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn record_evdev_read(&self, elapsed: Duration) {
        if self.enabled {
            let micros = elapsed.as_micros().min(u64::MAX as u128) as u64;
            self.evdev_read_count.fetch_add(1, Ordering::Relaxed);
            self.evdev_read_total_us
                .fetch_add(micros, Ordering::Relaxed);
            self.evdev_read_max_us.fetch_max(micros, Ordering::Relaxed);
        }
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

    pub fn record_android_socket(&self, elapsed: Duration) {
        if !self.enabled {
            return;
        }
        let micros = elapsed.as_micros().min(u64::MAX as u128) as u64;
        self.android_socket_count.fetch_add(1, Ordering::Relaxed);
        self.android_socket_total_us
            .fetch_add(micros, Ordering::Relaxed);
        self.android_socket_max_us
            .fetch_max(micros, Ordering::Relaxed);
        if elapsed >= Duration::from_millis(200) {
            self.android_socket_slow_200ms
                .fetch_add(1, Ordering::Relaxed);
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
        let window = last_report.elapsed();
        *last_report = Instant::now();

        let count = self.route_count.swap(0, Ordering::Relaxed);
        let buckets: [u64; HISTOGRAM_BUCKETS] =
            std::array::from_fn(|idx| self.route_histogram[idx].swap(0, Ordering::Relaxed));
        let queue_wait_count = self.queue_wait_count.swap(0, Ordering::Relaxed);
        let queue_wait_buckets: [u64; HISTOGRAM_BUCKETS] =
            std::array::from_fn(|idx| self.queue_wait_histogram[idx].swap(0, Ordering::Relaxed));
        let output_queue_wait_count = self.output_queue_wait_count.swap(0, Ordering::Relaxed);
        let output_queue_wait_buckets: [u64; HISTOGRAM_BUCKETS] = std::array::from_fn(|idx| {
            self.output_queue_wait_histogram[idx].swap(0, Ordering::Relaxed)
        });
        let evdev_to_send_count = self.evdev_to_send_count.swap(0, Ordering::Relaxed);
        let evdev_to_send_buckets: [u64; HISTOGRAM_BUCKETS] =
            std::array::from_fn(|idx| self.evdev_to_send_histogram[idx].swap(0, Ordering::Relaxed));
        let evdev_read_calls = self.evdev_read_count.swap(0, Ordering::Relaxed);
        let evdev_read_total_us = self.evdev_read_total_us.swap(0, Ordering::Relaxed);
        let backend_calls = self.backend_count.swap(0, Ordering::Relaxed);
        let backend_total_us = self.backend_total_us.swap(0, Ordering::Relaxed);
        let relay_calls = self.relay_count.swap(0, Ordering::Relaxed);
        let relay_total_us = self.relay_total_us.swap(0, Ordering::Relaxed);
        let android_socket_calls = self.android_socket_count.swap(0, Ordering::Relaxed);
        let android_socket_total_us = self.android_socket_total_us.swap(0, Ordering::Relaxed);
        let cpu_us = process_cpu_us();
        let previous_cpu_us = self.last_cpu_us.swap(cpu_us, Ordering::Relaxed);
        let cpu_percent = if window.is_zero() {
            0
        } else {
            ((cpu_us.saturating_sub(previous_cpu_us) as u128 * 100) / window.as_micros().max(1))
                as u64
        };
        tracing::info!(
            route_events = count,
            route_p50_us = percentile(&buckets, count, 50),
            route_p95_us = percentile(&buckets, count, 95),
            route_p99_us = percentile(&buckets, count, 99),
            queue_wait_events = queue_wait_count,
            queue_wait_p50_us = percentile(&queue_wait_buckets, queue_wait_count, 50),
            queue_wait_p95_us = percentile(&queue_wait_buckets, queue_wait_count, 95),
            queue_wait_p99_us = percentile(&queue_wait_buckets, queue_wait_count, 99),
            output_queue_wait_commands = output_queue_wait_count,
            output_queue_wait_p50_us =
                percentile(&output_queue_wait_buckets, output_queue_wait_count, 50),
            output_queue_wait_p95_us =
                percentile(&output_queue_wait_buckets, output_queue_wait_count, 95),
            output_queue_wait_p99_us =
                percentile(&output_queue_wait_buckets, output_queue_wait_count, 99),
            output_queue_depth_commands = self.output_queue_commands.load(Ordering::Relaxed),
            output_queue_high_water_commands =
                self.output_queue_high_water.swap(0, Ordering::Relaxed),
            evdev_to_android_send_events = evdev_to_send_count,
            evdev_to_android_send_p50_us =
                percentile(&evdev_to_send_buckets, evdev_to_send_count, 50),
            evdev_to_android_send_p95_us =
                percentile(&evdev_to_send_buckets, evdev_to_send_count, 95),
            evdev_to_android_send_p99_us =
                percentile(&evdev_to_send_buckets, evdev_to_send_count, 99),
            evdev_read_calls,
            evdev_read_avg_us = average(evdev_read_total_us, evdev_read_calls),
            evdev_read_max_us = self.evdev_read_max_us.swap(0, Ordering::Relaxed),
            queue_depth_events = self.queue_events.load(Ordering::Relaxed),
            queue_high_water_events = self.queue_high_water.swap(0, Ordering::Relaxed),
            syn_dropped = self.syn_dropped.swap(0, Ordering::Relaxed),
            max_epoll_drain_gap_us = self.drain_gap_max_us.swap(0, Ordering::Relaxed),
            backend_calls,
            backend_avg_us = average(backend_total_us, backend_calls),
            backend_max_us = self.backend_max_us.swap(0, Ordering::Relaxed),
            backend_stalls_over_200ms = self.backend_slow_200ms.swap(0, Ordering::Relaxed),
            android_socket_calls,
            android_socket_avg_us = average(android_socket_total_us, android_socket_calls),
            android_socket_max_us = self.android_socket_max_us.swap(0, Ordering::Relaxed),
            android_socket_stalls_over_200ms =
                self.android_socket_slow_200ms.swap(0, Ordering::Relaxed),
            desktop_relay_calls = relay_calls,
            desktop_relay_avg_us = average(relay_total_us, relay_calls),
            desktop_relay_max_us = self.relay_max_us.swap(0, Ordering::Relaxed),
            process_cpu_percent = cpu_percent,
            "input latency metrics (5s window)"
        );
    }
}

fn process_cpu_us() -> u64 {
    let mut cpu = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut cpu) } != 0 {
        return 0;
    }
    (cpu.tv_sec.max(0) as u64)
        .saturating_mul(1_000_000)
        .saturating_add((cpu.tv_nsec.max(0) as u64) / 1_000)
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
