use std::time::{Duration, Instant};
use url::Url;

/// Represents a single download source / mirror.
#[derive(Debug, Clone)]
pub struct Mirror {
    pub id: usize,
    pub url: Url,
    pub speed_ewma: f64,    // bytes per second
    pub ttfb_ewma_ms: f64,  // milliseconds
    pub in_flight: usize,
    pub failures: u32,
    pub consecutive_successes: u32,
    pub is_active: bool,
    pub cooldown_until: Option<Instant>,
}

impl Mirror {
    pub fn new(id: usize, url: Url) -> Self {
        Self {
            id,
            url,
            speed_ewma: 1_000_000.0, // Initial optimistic estimate: 1 MB/s
            ttfb_ewma_ms: 100.0,     // Initial optimistic TTFB: 100ms
            in_flight: 0,
            failures: 0,
            consecutive_successes: 0,
            is_active: true,
            cooldown_until: None,
        }
    }

    /// Calculates a composite fitness score. Higher is better.
    pub fn score(&self, now: Instant) -> f64 {
        if !self.is_active {
            return -1.0;
        }

        if let Some(cooldown) = self.cooldown_until {
            if now < cooldown {
                return -1.0;
            }
        }

        let speed_mbps = (self.speed_ewma / 1_000_000.0).max(0.01);
        let ttfb_s = (self.ttfb_ewma_ms / 1000.0).max(0.01);

        // Penalty for high in-flight concurrency to balance across mirrors
        let concurrency_penalty = 1.0 + (self.in_flight as f64 * 0.4);
        // Penalty for recent failures
        let failure_penalty = 1.0 + (self.failures as f64 * 2.0);

        speed_mbps / (ttfb_s * concurrency_penalty * failure_penalty)
    }

    pub fn record_progress(&mut self, bytes: u64, duration: Duration) {
        if duration.is_zero() || bytes == 0 {
            return;
        }
        let instant_speed = (bytes as f64) / duration.as_secs_f64();
        // EWMA alpha = 0.3
        const ALPHA: f64 = 0.3;
        self.speed_ewma = (ALPHA * instant_speed) + ((1.0 - ALPHA) * self.speed_ewma);
    }

    pub fn record_ttfb(&mut self, ttfb: Duration) {
        let ttfb_ms = ttfb.as_secs_f64() * 1000.0;
        const ALPHA: f64 = 0.25;
        self.ttfb_ewma_ms = (ALPHA * ttfb_ms) + ((1.0 - ALPHA) * self.ttfb_ewma_ms);
    }

    pub fn record_success(&mut self) {
        self.consecutive_successes += 1;
        if self.consecutive_successes > 3 && self.failures > 0 {
            self.failures -= 1;
        }
        self.cooldown_until = None;
    }

    pub fn record_failure(&mut self, now: Instant) {
        self.failures += 1;
        self.consecutive_successes = 0;
        // Exponential backoff: 2^failures seconds, up to 60s
        let backoff_secs = (1 << self.failures.min(6)).min(60);
        self.cooldown_until = Some(now + Duration::from_secs(backoff_secs));
    }
}

/// Orchestrates racing and dynamic load balancing across multiple mirrors / CDNs.
#[derive(Debug)]
pub struct MirrorRacer {
    mirrors: Vec<Mirror>,
}

impl MirrorRacer {
    pub fn new(urls: Vec<Url>) -> Self {
        let mirrors = urls
            .into_iter()
            .enumerate()
            .map(|(id, url)| Mirror::new(id, url))
            .collect();
        Self { mirrors }
    }

    pub fn mirrors(&self) -> &[Mirror] {
        &self.mirrors
    }

    pub fn mirrors_mut(&mut self) -> &mut [Mirror] {
        &mut self.mirrors
    }

    /// Selects the best performing mirror currently available.
    pub fn select_best_mirror(&self) -> Option<usize> {
        let now = Instant::now();
        self.mirrors
            .iter()
            .filter(|m| m.score(now) >= 0.0)
            .max_by(|a, b| {
                a.score(now)
                    .partial_cmp(&b.score(now))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|m| m.id)
    }

    pub fn get_mirror(&self, id: usize) -> Option<&Mirror> {
        self.mirrors.get(id)
    }

    pub fn get_mirror_mut(&mut self, id: usize) -> Option<&mut Mirror> {
        self.mirrors.get_mut(id)
    }

    pub fn acquire_mirror(&mut self, id: usize) {
        if let Some(m) = self.mirrors.get_mut(id) {
            m.in_flight += 1;
        }
    }

    pub fn release_mirror(&mut self, id: usize) {
        if let Some(m) = self.mirrors.get_mut(id) {
            m.in_flight = m.in_flight.saturating_sub(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mirror_racing_selection() {
        let urls = vec![
            Url::parse("https://fast.example.com/file.iso").unwrap(),
            Url::parse("https://slow.example.com/file.iso").unwrap(),
        ];
        let mut racer = MirrorRacer::new(urls);

        // Fast mirror: 10 MB/s, 20ms TTFB
        racer.get_mirror_mut(0).unwrap().speed_ewma = 10_000_000.0;
        racer.get_mirror_mut(0).unwrap().ttfb_ewma_ms = 20.0;

        // Slow mirror: 500 KB/s, 200ms TTFB
        racer.get_mirror_mut(1).unwrap().speed_ewma = 500_000.0;
        racer.get_mirror_mut(1).unwrap().ttfb_ewma_ms = 200.0;

        let best = racer.select_best_mirror().unwrap();
        assert_eq!(best, 0);

        // Simulate failure on fast mirror
        racer.get_mirror_mut(0).unwrap().record_failure(Instant::now());
        let best_after_fail = racer.select_best_mirror().unwrap();
        assert_eq!(best_after_fail, 1);
    }
}
