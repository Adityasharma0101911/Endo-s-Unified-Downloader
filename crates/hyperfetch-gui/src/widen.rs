//! More downloads at once than the "at once" setting while the line has room: when the downloads
//! running together take well under what the line was seen to carry (see
//! `hyperfetch_core::hosts::line`), as servers that each cap their speed leave it, and more wait,
//! one more may start, then after a pause another, a few at most; once they take most of the line,
//! none more. Running downloads are never stopped for it, and each still keeps to its hosts'
//! connection limits and caps, which the engine shares among all of them.

use std::time::{Duration, Instant};

/// At most this many more than "at once".
pub const MOST: usize = 3;
/// One more only while those running take under this share of the line...
const ROOM: f64 = 0.5;
/// ...and none more once they take this share of it.
const FULL: f64 = 0.8;
/// Between one more and the next, and after any download starts: time for its connections (and
/// the Autopilot's trials) to settle before the speed is judged.
pub const PAUSE: Duration = Duration::from_secs(15);

#[derive(Debug, Default)]
pub struct Widening {
    /// How many more than "at once" may run now.
    pub extra: usize,
    /// When `extra` last grew.
    grew: Option<Instant>,
}

impl Widening {
    /// How many more than `at_once` may run at `now`: `running` downloads run, `speed` bytes a
    /// second together, on a line of `line` (None: not known yet; the speed limit, when that is
    /// less), with a download `waiting` that would start in one more place or not, the last of
    /// them started at `started`. One at a time stays one at a time.
    #[allow(clippy::too_many_arguments)]
    pub fn update(&mut self, now: Instant, at_once: usize, running: usize, speed: f64, line: Option<f64>, waiting: bool, started: Option<Instant>) -> usize {
        let Some(line) = line.filter(|&line| line > 0.0 && at_once > 1) else {
            self.extra = 0;
            return 0;
        };
        let settled = |at: Option<Instant>| at.is_none_or(|at| now.saturating_duration_since(at) >= PAUSE);
        if speed >= FULL * line {
            self.extra = 0;
        } else if speed < ROOM * line && waiting && running >= at_once + self.extra && self.extra < MOST && settled(self.grew) && settled(started) {
            self.extra += 1;
            self.grew = Some(now);
        }
        self.extra
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINE: Option<f64> = Some(100e6);

    /// Under half the line, with downloads waiting and the last start settled, one more every
    /// pause, three at most; most of the line taken, none more at once.
    #[test]
    fn more_run_while_the_line_has_room() {
        let (mut w, t0) = (Widening::default(), Instant::now());
        let at = |secs: u64| t0 + Duration::from_secs(secs);
        assert_eq!(w.update(at(0), 4, 4, 30e6, LINE, true, Some(at(0))), 0, "the last start not settled yet");
        assert_eq!(w.update(at(15), 4, 4, 30e6, LINE, true, Some(at(0))), 1);
        assert_eq!(w.update(at(16), 4, 5, 30e6, LINE, true, Some(at(15))), 1, "one at a time, with a pause");
        assert_eq!(w.update(at(30), 4, 4, 30e6, LINE, true, Some(at(15))), 1, "the one more not taken yet");
        assert_eq!(w.update(at(30), 4, 5, 40e6, LINE, true, Some(at(15))), 2);
        assert_eq!(w.update(at(45), 4, 6, 45e6, LINE, true, Some(at(30))), 3);
        assert_eq!(w.update(at(90), 4, 7, 45e6, LINE, true, Some(at(45))), MOST, "a few at most");
        assert_eq!(w.update(at(91), 4, 7, 85e6, LINE, true, Some(at(45))), 0, "the line is full");
    }

    /// A queue whose downloads fill the line, one with nothing waiting, one at a time and a line
    /// not known yet never run more.
    #[test]
    fn a_normal_queue_runs_as_set() {
        let (mut w, t0) = (Widening::default(), Instant::now());
        for secs in (0..600).step_by(5) {
            let now = t0 + Duration::from_secs(secs);
            assert_eq!(w.update(now, 4, 4, 70e6, LINE, true, None), 0, "over half the line");
            assert_eq!(w.update(now, 4, 4, 10e6, LINE, false, None), 0, "nothing waits");
            assert_eq!(w.update(now, 1, 1, 10e6, LINE, true, None), 0, "one at a time");
            assert_eq!(w.update(now, 4, 4, 10e6, None, true, None), 0, "no line known");
            assert_eq!(w.update(now, 4, 3, 10e6, LINE, true, None), 0, "room left as set");
        }
    }
}
