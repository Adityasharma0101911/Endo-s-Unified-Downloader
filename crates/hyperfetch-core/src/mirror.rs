use std::time::{Duration, Instant};
use url::Url;

/// Consecutive bad responses (401/403/404/410, wrong Content-Range) after which a mirror is dropped.
const MAX_BAD_RESPONSES: u32 = 2;

/// Represents a single download source / mirror.
#[derive(Debug, Clone)]
pub struct Mirror {
    pub id: usize,
    pub url: Url,
    /// Validator this mirror issued (strong ETag, else Last-Modified), sent back as `If-Range`.
    pub if_range: Option<String>,
    pub speed_ewma: f64,    // bytes per second
    pub ttfb_ewma_ms: f64,  // milliseconds
    pub in_flight: usize,
    pub failures: u32,
    pub consecutive_successes: u32,
    pub bad_responses: u32,
    pub is_active: bool,
    pub cooldown_until: Option<Instant>,
    /// Concurrent connections the server tolerates; lowered when it throttles extra connections.
    pub max_connections: usize,
}

impl Mirror {
    pub fn new(id: usize, url: Url) -> Self {
        Self {
            id,
            url,
            if_range: None,
            speed_ewma: 1_000_000.0, // Initial optimistic estimate: 1 MB/s
            ttfb_ewma_ms: 100.0,     // Initial optimistic TTFB: 100ms
            in_flight: 0,
            failures: 0,
            consecutive_successes: 0,
            bad_responses: 0,
            is_active: true,
            cooldown_until: None,
            max_connections: usize::MAX,
        }
    }

    /// Calculates a composite fitness score. Higher is better; negative means "do not use now".
    pub fn score(&self, now: Instant) -> f64 {
        if !self.is_active || self.in_flight >= self.max_connections {
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

    /// The mirror answered a request with usable data. Also probes for one more connection
    /// if it throttled us earlier (additive increase).
    pub fn record_success(&mut self) {
        self.consecutive_successes += 1;
        if self.consecutive_successes > 3 && self.failures > 0 {
            self.failures -= 1;
        }
        self.bad_responses = 0;
        self.max_connections = self.max_connections.saturating_add(1);
    }

    /// A transient failure: lowers the mirror's score.
    pub fn record_failure(&mut self) {
        self.failures += 1;
        self.consecutive_successes = 0;
    }

    /// A response proving the mirror unusable. Returns true if the mirror was just deactivated.
    pub fn record_bad_response(&mut self) -> bool {
        self.record_failure();
        self.bad_responses += 1;
        let deactivate = self.is_active && self.bad_responses >= MAX_BAD_RESPONSES;
        if deactivate {
            self.is_active = false;
        }
        deactivate
    }

    /// The server rejected a connection (429/503) while `others_in_flight` of ours were being served.
    pub fn record_throttled(&mut self, others_in_flight: usize) {
        self.record_failure();
        self.max_connections = self.max_connections.min(others_in_flight.max(1));
    }

    /// Stops new requests to this mirror until `until`.
    pub fn cool_down(&mut self, until: Instant) {
        self.cooldown_until = Some(self.cooldown_until.map_or(until, |c| c.max(until)));
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

    /// Selects the best mirror that can take another connection right now, if any.
    pub fn select_best_mirror(&self) -> Option<usize> {
        let now = Instant::now();
        self.mirrors
            .iter()
            .map(|m| (m.id, m.score(now)))
            .filter(|&(_, score)| score >= 0.0)
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(id, _)| id)
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

    /// True once every mirror has been deactivated.
    pub fn all_inactive(&self) -> bool {
        self.mirrors.iter().all(|m| !m.is_active)
    }

    /// Connections currently open across all mirrors.
    pub fn in_flight(&self) -> usize {
        self.mirrors.iter().map(|m| m.in_flight).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn racer() -> MirrorRacer {
        MirrorRacer::new(vec![
            Url::parse("https://fast.example.com/file.iso").unwrap(),
            Url::parse("https://slow.example.com/file.iso").unwrap(),
        ])
    }

    #[test]
    fn test_mirror_racing_selection() {
        let mut racer = racer();

        // Fast mirror: 10 MB/s, 20ms TTFB
        racer.get_mirror_mut(0).unwrap().speed_ewma = 10_000_000.0;
        racer.get_mirror_mut(0).unwrap().ttfb_ewma_ms = 20.0;

        // Slow mirror: 500 KB/s, 200ms TTFB
        racer.get_mirror_mut(1).unwrap().speed_ewma = 500_000.0;
        racer.get_mirror_mut(1).unwrap().ttfb_ewma_ms = 200.0;

        assert_eq!(racer.select_best_mirror(), Some(0));

        // A cooling-down mirror is skipped, and success elsewhere must not lift the cooldown.
        racer.get_mirror_mut(0).unwrap().cool_down(Instant::now() + Duration::from_secs(60));
        racer.get_mirror_mut(0).unwrap().record_success();
        assert_eq!(racer.select_best_mirror(), Some(1));

        // With every mirror unavailable there is no fallback to mirror 0.
        racer.get_mirror_mut(1).unwrap().cool_down(Instant::now() + Duration::from_secs(60));
        assert_eq!(racer.select_best_mirror(), None);
    }

    #[test]
    fn test_bad_mirror_is_deactivated_after_consecutive_bad_responses() {
        let mut racer = racer();
        let m = racer.get_mirror_mut(0).unwrap();
        assert!(!m.record_bad_response());
        m.record_success(); // a good response in between resets the streak
        assert!(!m.record_bad_response());
        assert!(m.record_bad_response());
        assert!(!racer.all_inactive());
        assert_eq!(racer.select_best_mirror(), Some(1));
        assert!(!racer.get_mirror_mut(1).unwrap().record_bad_response());
        assert!(racer.get_mirror_mut(1).unwrap().record_bad_response());
        assert!(racer.all_inactive());
    }

    #[test]
    fn test_throttling_caps_connections_then_probes_upward() {
        let mut racer = MirrorRacer::new(vec![Url::parse("https://a.example.com/f").unwrap()]);
        racer.acquire_mirror(0);
        racer.acquire_mirror(0);
        racer.get_mirror_mut(0).unwrap().record_throttled(2);
        assert_eq!(racer.select_best_mirror(), None, "at the cap");
        racer.get_mirror_mut(0).unwrap().record_success();
        assert_eq!(racer.select_best_mirror(), Some(0), "one more connection is allowed after a success");
    }
}
