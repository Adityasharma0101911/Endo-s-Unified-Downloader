//! Autopilot: how many connections a download over ranges keeps open, found as it goes (see
//! `DownloadOptions::adaptive`), and why, in a line for the user.
//!
//! It starts where downloads from the host ended up fastest before, or from the probe's count, and
//! tries one change at a time, each measured over a window of a few times what a new request takes
//! to answer, after one such window for the change to settle: more connections, half as many again (twice as many while each one added kept its
//! speed), kept only when the download went faster by `GAIN`. When more did not pay, something
//! holds the download to a speed whatever its connections (the server, for each user, or the
//! user's line): fewer are tried, halving while they hold that speed, and it settles on the fewest
//! that reach it. Once settled it changes nothing more.
//!
//! A climb rather than a bandit sampling every count: the speed rises with connections up to a cap
//! and then stays flat or falls, so a climb finds that knee in a few windows, where sampling would
//! spend much of the download on counts that are worse.

use std::time::{Duration, Instant};

/// A change is kept when it makes the download this much faster.
const GAIN: f64 = 1.10;
/// A window lasts this many times what a new request takes to answer, within these bounds.
const STARTUPS: u32 = 4;
const MIN_WINDOW: Duration = Duration::from_millis(500);
const MAX_WINDOW: Duration = Duration::from_secs(5);
/// Connections added that each brought this share of what each one brought before kept their
/// speed: the next step doubles.
const HELD: f64 = 0.7;
/// A download going at this share of the user's line fills it.
const LINE_FULL: f64 = 0.9;
/// What the probe's read of a connection's start reads at most (see `engine::read_prefix`).
pub(crate) const SHOWN: u64 = 512 * 1024;
/// The first stream's speed, and one more connection's beside it, is read over windows each as
/// long as a new connection takes to answer (a few round trips), `SHOW_WINDOW` at least, or
/// `QUICK_WINDOW` (the engine's quickest look) once one holds `SHOW_BYTES`: fewer bytes
/// over less time are a few packets and a timer's tick, not a speed. The stream's is what it kept
/// over two windows in a row: one may hold a burst (what waited in buffers while the answer came,
/// or a server's idle credit). It is read once it stopped climbing, the fastest passed by none of
/// the `CALM` after it: a connection's speed grows while it starts, on a far host for seconds and
/// not evenly, and what it brings then says nothing of what it will. That fastest is its speed.
/// One after it is enough where a window lasts `CALM` of a new connection's answers: a near host's
/// stream climbs in a few of them, well within one. A window passes the fastest when it is more
/// than `CLIMB` faster: what one reading is off by from the next is no climb. At most `MAX_SHOW`
/// after the reading began, the fastest so far is taken, so a slow host still gets more. No window
/// is longer than `LONGEST_WINDOW`, however long the answer takes: a host far away has its speed
/// read in as many windows as a near one, not in seconds of one stream.
const SHOW_WINDOW: Duration = Duration::from_millis(100);
const QUICK_WINDOW: Duration = Duration::from_millis(10);
const LONGEST_WINDOW: Duration = Duration::from_millis(300);
const SHOW_BYTES: u64 = 1024 * 1024;
pub(crate) const CALM: u32 = 2;
pub(crate) const CLIMB: f64 = 0.05;
const MAX_SHOW: Duration = Duration::from_secs(5);
/// One more connection beside the first stream pays when the two together go this many times as
/// fast as the stream alone at its best: what one reading of a real line is off by, or the
/// stream's own climb, is no gain (see [`Autopilot::judge`]).
const PAY: f64 = 1.25;
/// The stream kept its speed beside one more when it went within this share of it.
const AGREE: f64 = 0.15;
/// More connections, each held to the rate one is, pay while what is left takes more than this
/// many of their set-ups at it (see `engine::read_prefix`); twice as many when nothing says they
/// bring more than the stream already gets.
const OPEN_SETUPS: f64 = 2.0;

/// What the Autopilot starts from: what the probe and earlier downloads found of the host.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Start {
    /// The download starts at the connections one from the host ended up fastest with.
    pub remembered: bool,
    /// One connection to the host keeps to this speed, as the probe saw (see `engine::RateWatch`):
    /// more may bring more, or share what one already gets.
    pub rate: Option<f64>,
    /// The speed the host held an earlier download to, whatever its connections.
    pub limit: Option<f64>,
    /// The user's line speed, once known (see `hosts::line`).
    pub line: Option<f64>,
    /// The host limits how often it is asked (see `hosts::limits_requests`): one connection, and
    /// no more are tried.
    pub limited: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Why {
    Starting,
    Remembered,
    /// Trying the width from this many, which went this fast.
    Trying(usize, f64),
    Most,
    Slower,
    /// More did not make it faster than this.
    Flat(f64),
    /// It fills the user's line, at this speed.
    Line(f64),
    Short,
    Limited,
    /// The first request's one stream, while it shows what it does.
    Alone,
    /// One more beside that stream, which went this fast, shows whether more pay.
    Step(f64),
    /// Opened from that stream, which went this fast, as one more made it faster.
    Opened(f64),
    /// Opened from that stream, each connection seen held to this rate.
    Held(f64),
    /// Opened from that stream, which kept this speed beside one more that was slow of its own.
    Beside(f64),
    /// New requests were answered too late, the streams they were cut from going on over their
    /// pieces first.
    Late,
}

#[derive(Debug)]
pub(crate) struct Autopilot {
    width: usize,
    max: usize,
    start: Start,
    /// When the window being measured began, and the bytes downloaded then.
    began: (Instant, u64),
    /// The window under way is not compared: the first, which the connections spend starting, and
    /// each after a change, which new connections spend starting and cut ones ending.
    settling: bool,
    /// The width a trial left, and how fast the download went there.
    trial: Option<(usize, f64)>,
    /// The next trial of more adds this many.
    step: usize,
    /// More did not pay: fewer are tried while they hold this speed.
    holding: Option<f64>,
    /// A trial of more paid.
    climbed: bool,
    /// The connection cap the host enforces, as last seen.
    cap: Option<usize>,
    /// Nothing more is tried.
    pub(crate) done: bool,
    /// The width the download goes fastest at, once found: not when too little was left to find it.
    pub(crate) settled: Option<usize>,
    why: Why,
    /// The fastest window, in bytes per second.
    pub(crate) best: f64,
    /// The download is its first request's one stream, and opens this many once more pay (see
    /// [`Autopilot::open`]); `None` once that is decided.
    opening: Option<usize>,
    /// The first stream's speed, read while it is alone.
    reading: Reading,
    /// One more connection is tried before the rest (see [`Autopilot::judge`]).
    judging: Option<Judging>,
}

/// A speed read over windows until it stops climbing (see `SHOW_WINDOW`).
#[derive(Debug, Clone, Copy)]
struct Reading {
    /// When the reading began, and the bytes in then.
    since: (Instant, u64),
    /// When the window under way began, and the bytes in then.
    window: (Instant, u64),
    /// The last window's speed.
    last: Option<f64>,
    /// The fastest speed kept over two windows, and the windows since that were no faster.
    best: Option<f64>,
    calm: u32,
}

impl Reading {
    fn new(now: Instant, downloaded: u64) -> Self {
        Self { since: (now, downloaded), window: (now, downloaded), last: None, best: None, calm: 0 }
    }

    /// Whether `got` bytes over `elapsed` make a window of `length` (see `SHOW_WINDOW`).
    fn long_enough(got: u64, elapsed: Duration, length: Duration) -> bool {
        let least = if got >= SHOW_BYTES { QUICK_WINDOW } else { SHOW_WINDOW };
        got > 0 && elapsed >= length.clamp(least, LONGEST_WINDOW)
    }

    /// The windows no faster than the fastest that show a speed stopped climbing, after one of
    /// `elapsed` from a host whose new connections take `length` to answer (see `CALM`).
    fn calm(elapsed: Duration, length: Duration) -> u32 {
        if elapsed >= CALM * length { 1 } else { CALM }
    }

    /// The speed of the window under way at `now`, with `downloaded` bytes in, once it is a window
    /// of `length`, which the next begins after, and how long it was. One that brought nothing yet
    /// stays open: a pause between a server's bursts is no speed.
    fn window(&mut self, now: Instant, downloaded: u64, length: Duration) -> Option<(f64, Duration)> {
        let (at, from) = self.window;
        let elapsed = now.saturating_duration_since(at);
        if !Self::long_enough(downloaded.saturating_sub(from), elapsed, length) {
            return None;
        }
        self.window = (now, downloaded);
        Some((downloaded.saturating_sub(from) as f64 / elapsed.as_secs_f64(), elapsed))
    }

    /// The speed kept over the window that ends at `now` and the one before, once a window of
    /// `length` ends (see [`Reading::window`]), and how long the last was.
    fn kept(&mut self, now: Instant, downloaded: u64, length: Duration) -> Option<(f64, Duration)> {
        let (speed, elapsed) = self.window(now, downloaded, length)?;
        self.last.replace(speed).map(|last| (last.min(speed), elapsed))
    }

    /// The speed at `now`, with `downloaded` bytes in, once it stopped climbing or `MAX_SHOW`
    /// passed: the fastest it kept.
    fn read(&mut self, now: Instant, downloaded: u64, length: Duration) -> Option<f64> {
        let (speed, elapsed) = self.kept(now, downloaded, length)?;
        match self.best {
            Some(best) if speed <= (1.0 + CLIMB) * best => (self.best, self.calm) = (Some(best.max(speed)), self.calm + 1),
            _ => (self.best, self.calm) = (Some(speed), 0),
        }
        self.best.filter(|_| self.calm >= Self::calm(elapsed, length) || now.saturating_duration_since(self.since.0) >= MAX_SHOW)
    }
}

/// One more connection beside the first stream, before the rest (see [`Autopilot::open`]).
#[derive(Debug, Clone, Copy)]
struct Judging {
    /// The width to open to if it pays.
    target: usize,
    /// The stream's speed alone: the fastest it went, alone or beside the new connection.
    base: f64,
    /// Why the width opens if it pays.
    paid: Why,
    /// The download since that connection's answer came.
    beside: Option<Beside>,
}

/// A moment of a download judging one more connection: when, with the download's bytes and its
/// first stream's in then.
type Mark = (Instant, u64, u64);

/// A download of its first stream and one more connection, window by window since that one's
/// answer (see [`Autopilot::judge`]).
#[derive(Debug, Clone, Copy)]
struct Beside {
    /// When the answer came.
    since: Instant,
    /// When the window under way began.
    window: Mark,
    /// The stream's speed over the last window, of its own bytes.
    last: Option<f64>,
    /// The new connection's fastest window, when it began and its speed of its own, and the
    /// windows since that were no faster.
    best: Option<(Mark, f64)>,
    calm: u32,
}

impl Autopilot {
    pub(crate) fn new(width: usize, max: usize, start: Start, now: Instant, downloaded: u64) -> Self {
        let max = if start.limited { 1 } else { max.max(1) };
        let width = width.clamp(1, max);
        Self {
            width,
            max,
            start,
            began: (now, downloaded),
            settling: true,
            trial: None,
            step: if start.rate.is_some() { width } else { (width / 2).max(1) },
            holding: None,
            climbed: false,
            cap: None,
            done: start.limited,
            settled: None,
            why: match start {
                Start { limited: true, .. } => Why::Limited,
                Start { remembered: true, .. } => Why::Remembered,
                _ => Why::Starting,
            },
            best: 0.0,
            opening: None,
            reading: Reading::new(now, downloaded),
            judging: None,
        }
    }

    /// Goes on as the download's first request, one stream which began at `since` with `from`
    /// bytes in, until it shows what it does: then it opens the width it was made with, or as
    /// many as some other rule says, if more pay (see [`Autopilot::open`]).
    pub(crate) fn alone(mut self, since: Instant, from: u64) -> Self {
        if !self.done {
            (self.opening, self.width, self.began, self.settling, self.why) = (Some(self.width), 1, (since, from), false, Why::Alone);
            self.reading = Reading::new(since, from);
        }
        self
    }

    /// Whether the download is still its first request's one stream (see [`Autopilot::alone`]).
    pub(crate) fn is_alone(&self) -> bool {
        self.opening.is_some()
    }

    /// Whether one more connection is being tried beside the first stream (see
    /// [`Autopilot::judge`]).
    pub(crate) fn is_judging(&self) -> bool {
        self.judging.is_some()
    }

    /// Its host was found to limit how often it is asked: the one stream goes on to the end.
    pub(crate) fn hold(&mut self) {
        (self.opening, self.judging, self.max, self.width, self.done, self.why) = (None, None, 1, 1, true, Why::Limited);
    }

    /// The host refused a request while the download widened, serving `took` of its connections:
    /// it keeps those, and no more are tried. The width, if it changes.
    pub(crate) fn took(&mut self, took: usize) -> Option<usize> {
        (self.opening, self.judging, self.trial, self.holding) = (None, None, None, None);
        self.cap = Some(took.max(1));
        self.settle(took.max(1), Why::Most)
    }

    /// The widening ended as the streams went on over the pieces of newcomers the host had yet to
    /// answer (see `Ramp::taken_back`): the download keeps the `took` connections that were
    /// answered, and goes no wider, but the host refused nothing and is not held to them.
    pub(crate) fn answered_late(&mut self, took: usize) -> Option<usize> {
        (self.opening, self.judging, self.trial, self.holding) = (None, None, None, None);
        self.max = took.max(1);
        self.settle(took.max(1), Why::Late)
    }

    /// While alone, the width to open at `now`, with `downloaded` bytes in and `left` to come, once
    /// the one stream has shown what it does; `startup` is what a new connection takes to answer
    /// (the host's recipe's, else the first request's), `cap` the connections the host serves at
    /// once. Once its speed stopped climbing (see `SHOW_WINDOW`), at that speed, which one more must
    /// raise (see [`Autopilot::judge`]); `rate`, when each connection to the host is seen held to
    /// one, says how many more could bring. Then the one stream goes on to the end, one request in
    /// all, if more would not pay: when what is left takes no more than `OPEN_SETUPS` of their
    /// set-ups at the rate, or twice as many at the stream's speed when nothing says more bring
    /// more (a set-up not known taken for `MIN_WINDOW`). Nor when the stream fills the line, or
    /// goes so near what the host held a download to before that one more could not make it `PAY`
    /// times as fast. Otherwise one more opens, which shows whether the rest pay: held to a rate,
    /// as many as fill the line (all it may, without the line known); else the width it was made
    /// with. Call it every few tens of milliseconds.
    pub(crate) fn open(&mut self, now: Instant, downloaded: u64, left: u64, startup: Option<Duration>, rate: Option<f64>, cap: Option<usize>) -> Option<usize> {
        let opening = self.opening?;
        self.cap = cap;
        let most = cap.map_or(self.max, |cap| cap.clamp(1, self.max));
        // Nothing in yet tells nothing: a stall is the stream's own to time out.
        let speed = self.reading.read(now, downloaded, startup.unwrap_or_default()).filter(|&speed| speed > 0.0)?;
        self.opening = None;
        self.best = self.best.max(speed);
        // Each held to the rate, more bring more: they pay once the rest takes over two of their
        // set-ups at it, as the probe's read of a capped connection judges. Else they may bring
        // nothing, which costs requests, not time: a margin twice that. A set-up measured however
        // short is the host's, not one unknown.
        let setup = startup.unwrap_or(MIN_WINDOW);
        let margin = match rate {
            Some(rate) => OPEN_SETUPS * rate,
            None => 2.0 * OPEN_SETUPS * speed,
        };
        let short = (left as f64) <= margin * setup.as_secs_f64();
        if short {
            (self.done, self.why) = (true, Why::Short);
            return None;
        }
        if self.fills_line(speed) {
            return self.settle(1, Why::Line(speed));
        }
        if self.start.limit.is_some_and(|limit| speed * PAY >= limit) {
            return self.settle(1, Why::Flat(speed));
        }
        // Held to a rate, more bring more, unless what holds the stream holds them all (a server's
        // limit for each user looks the same from one stream): as many as fill the line.
        let (target, paid) = match rate {
            Some(rate) => {
                self.start.rate = Some(rate);
                (self.start.line.map_or(most, |line| (line / rate).ceil() as usize), Why::Held(rate))
            }
            None => (opening, Why::Opened(speed)),
        };
        let target = target.clamp(1, most);
        if target < 2 {
            return self.settle(1, Why::Most);
        }
        // One more first: the rest only if it makes the download faster.
        self.judging = Some(Judging { target, base: speed, paid, beside: None });
        (self.width, self.why) = (2, Why::Step(speed));
        Some(2)
    }

    /// While one more connection is tried beside the first stream (see [`Autopilot::open`]): what
    /// the stream does alone, which every gain is measured against, is the fastest it kept over
    /// two windows, alone until that connection's answer (`answered`), or beside it after. From
    /// then on, the download's bytes (`downloaded`) and the stream's (`first`) are read over
    /// windows as long as `startup` makes them (see `SHOW_WINDOW`), and what the new connection
    /// brings of its own is what the stream did not: a young one takes little from the stream yet
    /// and brings little, so nothing is decided until its own speed stopped climbing, or
    /// `MAX_SHOW` after the answer. Over the windows from its fastest on, the width it goes to:
    /// all it was to open when the two together went `PAY` times as fast as the stream alone, for
    /// the climb to go on from, or when they did not but the stream kept its speed beside the new
    /// connection, which was then slow of its own and not held with it: it faded to half its
    /// fastest or less (one cut off or fading out), where one on a full line climbs; else
    /// the stream alone, the line or the host being what holds them both, which is kept as the
    /// host's limit.
    pub(crate) fn judge(&mut self, now: Instant, downloaded: u64, first: u64, answered: bool, startup: Duration) -> Option<usize> {
        let judging = self.judging.as_mut()?;
        if !answered {
            judging.beside = None;
            if let Some((speed, _)) = self.reading.kept(now, downloaded, startup) {
                judging.base = judging.base.max(speed);
            }
            return None;
        }
        let beside = judging.beside.get_or_insert(Beside { since: now, window: (now, downloaded, first), last: None, best: None, calm: 0 });
        let (at, all, own) = beside.window;
        let elapsed = now.saturating_duration_since(at);
        if !Reading::long_enough(downloaded.saturating_sub(all), elapsed, startup) {
            return None;
        }
        beside.window = (now, downloaded, first);
        let alone = first.saturating_sub(own) as f64 / elapsed.as_secs_f64();
        let newcomer = downloaded.saturating_sub(all) as f64 / elapsed.as_secs_f64() - alone;
        // What the stream kept over two windows, as alone (see `SHOW_WINDOW`).
        if let Some(last) = beside.last.replace(alone) {
            judging.base = judging.base.max(last.min(alone));
        }
        match beside.best {
            Some((_, best)) if newcomer <= (1.0 + CLIMB) * best => beside.calm += 1,
            _ => (beside.best, beside.calm) = (Some(((at, all, own), newcomer)), 0),
        }
        let settled = beside.calm >= Reading::calm(elapsed, startup) || now.saturating_duration_since(beside.since) >= MAX_SHOW;
        let Some(((at, all, own), fastest)) = beside.best.filter(|_| settled) else {
            return None;
        };
        let secs = now.saturating_duration_since(at).as_secs_f64();
        let (both, kept) = (downloaded.saturating_sub(all) as f64 / secs, first.saturating_sub(own) as f64 / secs);
        let Judging { target, base, paid, .. } = *judging;
        self.judging = None;
        self.best = self.best.max(both);
        if both >= PAY * base {
            return self.open_to(target, now, downloaded, paid);
        }
        // Slow of its own, the new connection faded after its start (cut off, or fading out): one
        // that arrives on a line the stream fills gets little and climbs slowly, taking from the
        // stream what it gains, and what the stream loses is then too little to tell.
        if kept * (1.0 + AGREE) >= base && both - kept <= fastest / 2.0 {
            return self.open_to(target, now, downloaded, Why::Beside(base));
        }
        let why = self.held(both.max(base));
        self.settle(1, why)
    }

    /// The first stream reached the piece of the connection beside it before the verdict, and
    /// took it back (see `ChunkManager::overtaken`): the two did not make it faster, and the
    /// stream goes on alone, the host kept as one stream fills. The width, if it changes.
    pub(crate) fn overtaken(&mut self) -> Option<usize> {
        let base = self.judging.take()?.base;
        self.best = self.best.max(base);
        let why = self.held(base);
        self.settle(1, why)
    }

    /// Opens `width` from the one stream at `now`, with `downloaded` bytes in, for `why`: the
    /// climb goes on from there, its first window not compared. The width, if more than the
    /// stream.
    fn open_to(&mut self, width: usize, now: Instant, downloaded: u64, why: Why) -> Option<usize> {
        self.width = width;
        self.step = if self.start.rate.is_some() { width } else { (width / 2).max(1) };
        (self.began, self.settling, self.why) = ((now, downloaded), true, why);
        (width > 1).then_some(width)
    }

    /// The speed more connections did not raise the download past, once it settled on that: the
    /// most its host serves one user, unless that is the user's line.
    pub(crate) fn limit(&self) -> Option<f64> {
        match self.why {
            Why::Flat(speed) if !self.capped() => Some(speed),
            _ => None,
        }
    }

    /// Keeps to at most `most` connections from now on, the count the user set for the download:
    /// at once when it has more, a trial under way given up; one that settled on the old ceiling
    /// climbs again under a higher one. The width, if it changes.
    pub(crate) fn cap_at(&mut self, most: usize) -> Option<usize> {
        self.max = most.max(1);
        if self.width > self.max {
            self.width = self.max;
            (self.trial, self.holding, self.settling) = (None, None, true);
            return Some(self.width);
        }
        if self.done && self.why == Why::Most && self.width < self.max {
            (self.done, self.settled, self.settling) = (false, None, true);
        }
        None
    }

    /// The download waits for the network at `now`, with `downloaded` bytes in: the window under
    /// way is not compared, nor the next, which its connections spend starting again.
    pub(crate) fn waiting(&mut self, now: Instant, downloaded: u64) {
        (self.began, self.settling) = ((now, downloaded), true);
        self.reading = Reading::new(now, downloaded);
        if let Some(judging) = self.judging.as_mut() {
            judging.beside = None;
        }
    }

    /// The width to go on at, when it changes, with `downloaded` bytes in at `now` and `left` to
    /// come; `startup` is what a new request takes to answer, `cap` the connections the host serves
    /// at once (see `hosts`). Call it every few hundred milliseconds.
    pub(crate) fn next(&mut self, now: Instant, downloaded: u64, left: u64, startup: Duration, cap: Option<usize>) -> Option<usize> {
        self.cap = cap;
        let window = startup.saturating_mul(STARTUPS).clamp(MIN_WINDOW, MAX_WINDOW);
        let (began, from) = self.began;
        let elapsed = now.saturating_duration_since(began);
        if self.done || elapsed < window {
            return None;
        }
        self.began = (now, downloaded);
        let speed = downloaded.saturating_sub(from) as f64 / elapsed.as_secs_f64();
        self.best = self.best.max(speed);
        if std::mem::take(&mut self.settling) {
            return None;
        }
        // Each one more must have the time to show what it brings.
        let short = (left as f64) < 3.0 * speed * window.as_secs_f64();
        if let Some((from, before)) = self.trial.take() {
            match self.holding {
                Some(holds) if speed * GAIN >= holds && self.width > 1 && !short => return self.try_width(self.width / 2, speed),
                Some(holds) if speed * GAIN >= holds => return self.settle(self.width, self.held(holds)),
                Some(holds) => return self.settle(from, self.held(holds)),
                None if speed >= before * GAIN => {
                    let each = (speed - before) / (self.width - from) as f64;
                    self.step = if each >= HELD * before / from as f64 { self.width } else { (self.width / 2).max(1) };
                    self.climbed = true;
                }
                None => {
                    self.width = from;
                    return self.fewer(before, speed < before / GAIN, short).or(Some(from));
                }
            }
        }
        // Nothing moved: nothing to compare one more with.
        if speed <= 0.0 {
            return None;
        }
        // Together no faster than half of them would be at the speed one had alone: a cap they
        // share, which the fewest that reach this speed reach.
        if let Some(rate) = self.start.rate.take().filter(|_| !self.climbed && !self.start.remembered && !short) {
            let fewest = (speed / (rate * GAIN)).ceil().max(1.0) as usize;
            if fewest * 2 <= self.width {
                self.holding = Some(speed);
                return self.try_width(fewest, speed);
            }
        }
        let most = cap.map_or(self.max, |cap| cap.clamp(1, self.max));
        if self.fills_line(speed) {
            return self.fewer(speed, false, short);
        }
        if self.width >= most {
            return self.settle(self.width, Why::Most);
        }
        if short {
            self.done = true;
            self.why = Why::Short;
            return None;
        }
        self.try_width((self.width + self.step).min(most), speed)
    }

    /// After more did not pay over `speed`, or the line is full at it: fewer are tried, unless more
    /// made it `slower`, the width came from what paid or from the host's past, or nothing is left
    /// to be had.
    fn fewer(&mut self, speed: f64, slower: bool, short: bool) -> Option<usize> {
        let why = if slower { Why::Slower } else { self.held(speed) };
        if slower || self.climbed || self.width == 1 || self.start.remembered || self.start.limit.is_some() || short {
            return self.settle(self.width, why);
        }
        self.holding = Some(speed);
        self.try_width(self.width / 2, speed)
    }

    fn try_width(&mut self, to: usize, speed: f64) -> Option<usize> {
        self.trial = Some((self.width, speed));
        self.why = Why::Trying(self.width, speed);
        self.width = to.max(1);
        self.settling = true;
        Some(self.width)
    }

    /// Settles on `width`, or the host's connection cap if that is fewer, for `why`; the width,
    /// if it changes.
    fn settle(&mut self, width: usize, why: Why) -> Option<usize> {
        let width = self.cap.map_or(width, |cap| width.min(cap.max(1)));
        let changed = width != self.width;
        self.width = width;
        self.done = true;
        self.settled = Some(width);
        self.why = why;
        changed.then_some(width)
    }

    /// Why the download holds at `speed` whatever its connections.
    fn held(&self, speed: f64) -> Why {
        if self.fills_line(speed) { Why::Line(speed) } else { Why::Flat(speed) }
    }

    fn fills_line(&self, speed: f64) -> bool {
        self.start.line.is_some_and(|line| speed >= LINE_FULL * line)
    }

    /// Whether the host's connection cap holds the download to fewer than its width.
    fn capped(&self) -> bool {
        self.cap.is_some_and(|cap| cap <= self.width)
    }

    /// Why the download has the connections it has, in a line for the user.
    pub(crate) fn why(&self) -> String {
        let shown = self.cap.map_or(self.width, |cap| cap.clamp(1, self.width));
        let c = connections(shown);
        if self.capped() {
            return format!("{c}: the server refused more at once (429/503), so it gets no more");
        }
        match self.why {
            Why::Starting => format!("{c} to start; more are tried while they make it faster"),
            Why::Remembered => format!("{c}: downloads from this site were fastest with that"),
            Why::Trying(was, speed) => format!("Trying {c}: {} went at {}", connections(was), per_sec(speed)),
            Why::Most if self.climbed => format!("{c}: each one added speed, and that is the most it may open"),
            Why::Most => format!("{c}: the most it may open"),
            Why::Slower => format!("{c}: more made it slower"),
            Why::Flat(speed) if self.start.line.is_some() => {
                format!("{c}: the server limits each user to about {}, so more would not help", per_sec(speed))
            }
            Why::Flat(speed) => format!("{c}: the server or your line holds it to about {}, so more would not help", per_sec(speed)),
            Why::Line(speed) => format!("{c}: that fills your line (about {})", per_sec(speed)),
            Why::Short => format!("{c}: too little is left to try more"),
            Why::Late => format!("{c}: the server answers new requests too late for more to pay"),
            Why::Limited => format!("{c}: the server limits how often it is asked (429/503), so it is asked for the file alone"),
            Why::Alone => format!("{c} while its speed shows; more are opened only if they pay"),
            Why::Step(speed) => format!("{c}: the first went at {}; a second shows whether more pay", per_sec(speed)),
            Why::Opened(speed) => {
                format!("{c}: the first went at {}, and a second made it faster; more are tried while they make it faster", per_sec(speed))
            }
            Why::Held(rate) => format!("{c}: each connection is held to {}, so more bring more", per_sec(rate)),
            Why::Beside(speed) => format!(
                "{c}: the first kept its {} beside a second, which was slow of its own; more are tried while they make it faster",
                per_sec(speed)
            ),
        }
    }
}

/// What the Autopilot says of a download it does not steer, which keeps `width` connections:
/// the user turned it off, or a speed `limited` it.
pub(crate) fn off(width: usize, limited: bool) -> String {
    if limited {
        "Off: a speed limit is set, so more connections would not help".to_string()
    } else {
        format!("Off: keeps the {} it started with", connections(width))
    }
}

/// What the Autopilot says of a download it does not steer whose connections the user set.
pub(crate) fn set(width: usize) -> String {
    format!("Off: {}, as you set for this download", connections(width))
}

fn connections(n: usize) -> String {
    if n == 1 { "1 connection".to_string() } else { format!("{n} connections") }
}

fn per_sec(rate: f64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    if rate >= MIB { format!("{:.1} MiB/s", rate / MIB) } else { format!("{:.0} KiB/s", rate / 1024.0) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::netsim::Model;

    const MB: f64 = 1e6;

    /// Steps `pilot` through `windows` windows of a second each (four startups of 250 ms), each
    /// going at what `model` reads for its width, with the host's connection cap learned once a
    /// width went past it, as a refusal teaches it (see `hosts`). The widths it went through.
    fn drive(pilot: &mut Autopilot, model: &mut Model, windows: u64) -> Vec<usize> {
        let (t0, mut downloaded, mut cap) = (pilot.began.0, pilot.began.1, None);
        let mut widths = vec![pilot.width];
        for n in 1..=windows {
            if pilot.width > model.max_connections {
                cap = Some(model.max_connections);
            }
            downloaded += model.speed(pilot.width) as u64;
            pilot.next(t0 + Duration::from_secs(n), downloaded, 1 << 40, Duration::from_millis(250), cap);
            widths.push(pilot.width);
        }
        assert!(pilot.done, "settled within {windows} windows: {widths:?}");
        widths.dedup();
        widths
    }

    fn pilot(width: usize, max: usize, start: Start) -> Autopilot {
        Autopilot::new(width, max, start, Instant::now(), 0)
    }

    /// While the download waits for the network nothing is compared: a trial under way is neither
    /// found slower nor settled on, so no poor count is kept for the host; it goes on after.
    #[test]
    fn test_a_wait_for_the_network_is_no_window() {
        let (startup, mut pilot) = (Duration::from_millis(250), pilot(4, 32, Start::default()));
        let (t0, mut downloaded) = (pilot.began.0, 0);
        for n in 1..=2 {
            downloaded += 4 * MB as u64;
            pilot.next(t0 + Duration::from_secs(n), downloaded, 1 << 40, startup, None);
        }
        assert_eq!(pilot.width, 6, "a trial of more: {}", pilot.why());
        // Three seconds without the network, told every tick; nothing comes in.
        for tick in 1..=20 {
            let now = t0 + Duration::from_secs(2) + Duration::from_millis(150 * tick);
            pilot.waiting(now, downloaded);
            pilot.next(now, downloaded, 1 << 40, startup, None);
        }
        assert!(!pilot.done && pilot.width == 6, "{}", pilot.why());
        // Back: the start is not compared, then six go faster than four did.
        for n in 1..=2 {
            downloaded += 6 * MB as u64;
            pilot.next(t0 + Duration::from_secs(5 + n), downloaded, 1 << 40, startup, None);
        }
        assert!(pilot.settled.is_none() && pilot.width > 6, "{}", pilot.why());
    }

    /// Over any seed: what more connections bring nothing past, a cap for each user, is reached
    /// with the fewest that reach it, found by halving once one more brought nothing; and it is
    /// kept, as the host's limit.
    #[test]
    fn test_a_cap_for_each_user_settles_on_the_fewest_connections_that_reach_it() {
        for seed in 1..=20 {
            let mut model = Model { per_connection: 2.0 * MB, per_client: 4.0 * MB, seed, ..Default::default() };
            let mut pilot = pilot(8, 32, Start::default());
            assert_eq!(drive(&mut pilot, &mut model, 30), [8, 12, 4, 2, 1, 2], "seed {seed}");
            assert_eq!(pilot.settled, Some(2));
            assert!(pilot.limit().is_some_and(|limit| (limit / (4.0 * MB) - 1.0).abs() < 0.05), "{:?}", pilot.limit());
            let why = pilot.why();
            assert!(why.starts_with("2 connections: the server or your line holds it to about ") && why.ends_with(" MiB/s, so more would not help"), "{why}");
        }
        // The probe's one connection went as fast as all of them do: straight down to as few.
        for (per_client, widths) in [(4.0 * MB, [16, 2, 1, 2]), (8.0 * MB, [16, 4, 2, 4])] {
            let mut model = Model { per_connection: 2.0 * MB, per_client, ..Default::default() };
            let mut pilot = pilot(16, 16, Start { rate: Some(2.0 * MB), ..Default::default() });
            assert_eq!(drive(&mut pilot, &mut model, 20), widths);
        }
    }

    /// A window lasts four times what a new request takes to answer, half a second at least; the
    /// first, and the one after each change, are not compared, as connections start in them.
    #[test]
    fn test_windows_are_four_startups_long() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut pilot = Autopilot::new(2, 8, Start::default(), t0, 0);
        let fast = Duration::from_millis(10);
        assert_eq!(pilot.next(at(400), 1_000_000, 1 << 40, fast, None), None, "windows are at least 500 ms");
        assert_eq!(pilot.next(at(500), 1_000_000, 1 << 40, fast, None), None, "the first window is the start");
        assert_eq!(pilot.next(at(1000), 2_000_000, 1 << 40, fast, None), Some(3));
        assert_eq!(pilot.next(at(1500), 2_100_000, 1 << 40, fast, None), None, "the change settles");
        assert_eq!(pilot.next(at(2000), 2_200_000, 1 << 40, fast, None), Some(2), "then it is judged: slower");
        let slow = Duration::from_secs(1);
        let mut pilot = Autopilot::new(1, 8, Start::default(), t0, 0);
        pilot.next(at(4000), 1_000_000, 1 << 40, slow, None);
        assert_eq!(pilot.next(at(7999), 2_000_000, 1 << 40, slow, None), None, "four startups make a window");
        assert_eq!(pilot.next(at(8000), 2_000_000, 1 << 40, slow, None), Some(2));
    }

    /// A cap on each connection: twice as many while each one added keeps its speed, up to the
    /// most allowed, and no further.
    #[test]
    fn test_a_cap_on_each_connection_climbs_to_the_most_allowed() {
        let mut model = Model { per_connection: 1.0 * MB, ..Default::default() };
        let mut pilot = pilot(4, 32, Start::default());
        assert_eq!(drive(&mut pilot, &mut model, 20), [4, 6, 12, 24, 32]);
        assert_eq!((pilot.settled, pilot.limit()), (Some(32), None));
        assert_eq!(pilot.why(), "32 connections: each one added speed, and that is the most it may open");

        // The probe saw the cap: it doubles from the start.
        let mut pilot = self::pilot(4, 32, Start { rate: Some(1.0 * MB), ..Default::default() });
        assert_eq!(drive(&mut pilot, &mut model, 20), [4, 8, 16, 32]);

        // Up to where more stop paying: the last width that paid.
        let mut model = Model { per_connection: 1.0 * MB, per_client: 10.0 * MB, ..Default::default() };
        let mut pilot = self::pilot(4, 64, Start::default());
        let widths = drive(&mut pilot, &mut model, 20);
        assert!(widths[..3] == [4, 6, 12] && widths[3] > 12 && widths[4..] == [12], "{widths:?}");
        assert!(pilot.limit().is_some_and(|limit| (limit / (10.0 * MB) - 1.0).abs() < 0.05));
    }

    /// The count the user set is the ceiling: a climb stops at it, one that settled there climbs
    /// again under a higher one, and a lower one takes the width down at once.
    #[test]
    fn test_the_users_count_is_the_ceiling() {
        let mut model = Model { per_connection: 1.0 * MB, ..Default::default() };
        let mut pilot = pilot(4, 32, Start::default());
        assert_eq!(pilot.cap_at(8), None, "it has fewer");
        assert_eq!(drive(&mut pilot, &mut model, 20), [4, 6, 8]);
        assert_eq!(pilot.why(), "8 connections: each one added speed, and that is the most it may open");
        assert_eq!(pilot.cap_at(16), None);
        assert!(!pilot.done);
        assert_eq!(drive(&mut pilot, &mut model, 20).last(), Some(&16));
        assert_eq!((pilot.cap_at(2), pilot.width), (Some(2), 2));
    }

    /// The host's cap on connections at once bounds every trial once it is seen; the download
    /// stays at it and says why.
    #[test]
    fn test_a_connection_cap_is_never_tried_past_once_seen() {
        let mut model = Model { per_connection: 1.0 * MB, max_connections: 6, ..Default::default() };
        let mut pilot = pilot(4, 32, Start::default());
        assert_eq!(drive(&mut pilot, &mut model, 20), [4, 6, 12, 6]);
        assert_eq!((pilot.settled, pilot.limit()), (Some(6), None), "the cap, not a speed limit, held it");
        assert_eq!(pilot.why(), "6 connections: the server refused more at once (429/503), so it gets no more");
    }

    /// A line one connection fills: no more are added, and several are brought down to one.
    #[test]
    fn test_a_full_line_gets_no_more_connections() {
        let line = Start { line: Some(10.0 * MB), ..Default::default() };
        let mut model = Model { per_connection: 12.0 * MB, line: 10.0 * MB, ..Default::default() };
        let mut one = pilot(1, 16, line);
        assert_eq!(drive(&mut one, &mut model, 10), [1]);
        assert!(one.why().starts_with("1 connection: that fills your line (about "), "{}", one.why());
        assert_eq!(one.limit(), None, "the line, not the host");
        let mut four = pilot(4, 16, line);
        assert_eq!(drive(&mut four, &mut model, 10), [4, 2, 1]);
    }

    /// Reads `pilot` alone every 50 ms up to `until` ms of a stream that brings `bytes(ms)` by
    /// then, `left` to come, each connection seen held to `rate` if that is given: when it opened
    /// (ms) and to what, if it did.
    fn opened(pilot: &mut Autopilot, t0: Instant, until: u64, left: u64, rate: Option<f64>, bytes: impl Fn(u64) -> u64) -> Option<(u64, usize)> {
        let startup = Duration::from_millis(10);
        (1..=until / 50).map(|n| n * 50).find_map(|ms| pilot.open(t0 + Duration::from_millis(ms), bytes(ms), left, Some(startup), rate, None).map(|width| (ms, width)))
    }

    /// The first request's one stream opens more only once its speed stopped climbing (its best
    /// window passed by neither of the two after it), and only if they pay: one more first, which
    /// opens the rest if it makes the download faster (see [`Autopilot::judge`]). The rest are as
    /// many as fill the line when each connection is seen held to a rate, else the width it was
    /// made with. None when the stream fills the line, goes near what the host held a download to
    /// before, or would end before more could show what they bring.
    #[test]
    fn test_one_stream_opens_more_only_once_they_pay() {
        let (t0, startup) = (Instant::now(), Duration::from_millis(10));
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let alone = |start: Start| Autopilot::new(8, 16, start, t0, 0).alone(t0, 0);
        let target = |pilot: &Autopilot| pilot.judging.map(|judging| judging.target);
        let each = Some(2.0 * MB);
        let mut pilot = alone(Start::default());
        assert_eq!((pilot.width, pilot.is_alone()), (1, true));
        assert_eq!(pilot.why(), "1 connection while its speed shows; more are opened only if they pay");
        assert_eq!(opened(&mut pilot, t0, 1000, 1 << 30, each, |ms| ms * 2_000), Some((300, 2)), "held to a rate: one more first");
        assert_eq!(target(&pilot), Some(16), "then all it may");
        assert_eq!(pilot.judge(at(300), 600_000, 600_000, true, startup), None);
        assert_eq!(pilot.judge(at(310), 640_000, 620_000, true, startup), None, "too soon to tell");
        assert_eq!(pilot.judge(at(400), 1_000_000, 800_000, true, startup), None, "its first window, 2 MB/s of its own");
        assert_eq!(pilot.judge(at(500), 1_400_000, 1_000_000, true, startup), Some(16), "one no faster from a near host: all it may");
        assert_eq!(pilot.why(), "16 connections: each connection is held to 1.9 MiB/s, so more bring more");
        assert!(!pilot.is_alone() && pilot.next(at(1000), 4_000_000, 1 << 30, startup, None).is_none(), "its first window is not compared");
        let mut pilot = alone(Start { line: Some(10.0 * MB), ..Default::default() });
        assert_eq!(opened(&mut pilot, t0, 1000, 1 << 30, each, |ms| ms * 2_000), Some((300, 2)));
        assert_eq!(target(&pilot), Some(5), "as many as fill the line");
        let mut capped = alone(Start::default());
        let at_cap = (1..=20).map(|n| n * 50).find_map(|ms| capped.open(at(ms), ms * 2_000, 1 << 30, Some(startup), each, Some(3)));
        assert_eq!((at_cap, target(&capped)), (Some(2), Some(3)), "the host's cap");
        // Held to a rate, but done within two set-ups of new connections: the one stream.
        let mut soon = alone(Start::default());
        assert_eq!((opened(&mut soon, t0, 1000, 30_000, each, |ms| ms * 2_000), soon.done, soon.width), (None, true, 1));
        // Held near the speed the host held a download to before, whatever its connections, one
        // more could not make it `PAY` times as fast: no more, as where a second did not pay.
        let mut known = alone(Start { limit: Some(2.1 * MB), ..Default::default() });
        assert_eq!((opened(&mut known, t0, 1000, 1 << 30, each, |ms| ms * 2_000), known.settled), (None, Some(1)));
        let mut known = alone(Start { limit: Some(2.4 * MB), ..Default::default() });
        assert_eq!((opened(&mut known, t0, 1000, 1 << 30, None, |ms| ms * 2_000), known.settled), (None, Some(1)));

        // Not held to a rate: three windows of 100 ms, each ten answers long, the speed kept over
        // the first two not passed in the next, then one more; 8 if it pays.
        let mut pilot = alone(Start::default());
        assert_eq!(opened(&mut pilot, t0, 1000, 1 << 30, None, |ms| ms * 10_000), Some((300, 2)), "a long way to go");
        assert!(pilot.is_judging() && !pilot.is_alone() && target(&pilot) == Some(8), "{}", pilot.why());
        assert_eq!(pilot.why(), "2 connections: the first went at 9.5 MiB/s; a second shows whether more pay");
        // Over a LAN, a MiB and more in each 10 ms, answers in 1 ms: windows of 10 ms, one after
        // the best enough. At 2.5 GB/s, 128 MiB take 54 ms: more open while there is some left.
        let mut fast = alone(Start::default());
        let lan_setup = Some(Duration::from_millis(1));
        let found = (1..=20).map(|n| n * 10).find_map(|ms| fast.open(at(ms), ms * 2_500_000, 1 << 34, lan_setup, None, None).map(|width| (ms, width)));
        assert_eq!(found, Some((30, 2)));
        // A set-up measured as next to nothing is the host's, so 64 MiB at 300 MB/s pay for more;
        // one not known is taken for 0.5 s, in which the stream brings more than is left.
        let lan = |setup: Option<Duration>| {
            let mut pilot = alone(Start::default());
            ((1..=20).map(|n| n * 20).find_map(|ms| pilot.open(at(ms), ms * 300_000, 64 << 20, setup, None, None)), pilot.why())
        };
        assert_eq!(lan(Some(Duration::from_micros(200))).0, Some(2));
        assert_eq!(lan(None), (None, "1 connection: too little is left to try more".to_string()));
        let mut short = alone(Start::default());
        assert_eq!(opened(&mut short, t0, 1000, 300_000, None, |ms| ms * 10_000), None, "four set-ups of 10 ms take 400 KB");
        assert!(short.done && short.width == 1 && short.settled.is_none(), "{}", short.why());
        assert_eq!(short.why(), "1 connection: too little is left to try more");
        let mut full = alone(Start { line: Some(10.0 * MB), ..Default::default() });
        assert_eq!((opened(&mut full, t0, 1000, 1 << 30, None, |ms| ms * 10_000), full.done, full.settled), (None, true, Some(1)));
        assert!(full.why().starts_with("1 connection: that fills your line"), "{}", full.why());
        let mut held = alone(Start { limit: Some(10.0 * MB), ..Default::default() });
        assert_eq!((opened(&mut held, t0, 1000, 1 << 30, None, |ms| ms * 10_000), held.settled), (None, Some(1)));
        // A host that limits how often it is asked is never more than the stream.
        let limited = alone(Start { limited: true, ..Default::default() });
        assert!(!limited.is_alone() && limited.done && limited.width == 1);
        let mut found = alone(Start::default());
        found.hold();
        assert!(!found.is_alone() && found.done && found.width == 1, "{}", found.why());
        assert!(found.why().contains("limits how often it is asked"));
    }

    /// A stream from a far host grows for a second before it reaches its speed: what decides is
    /// the speed it reached, not what it brought while it started. Here that fills the line, so
    /// nothing more opens, where its first quarter second would have said a tenth of it.
    #[test]
    fn test_a_stream_is_judged_by_the_speed_it_reaches() {
        let t0 = Instant::now();
        // Doubling every 200 ms from 0.5 MB/s, up to 8 MB/s from 800 ms on.
        let rate = |ms: u64| 0.5 * MB * 2f64.powi((ms / 200).min(4) as i32);
        let bytes = |ms: u64| (0..ms).map(|t| rate(t) / 1000.0).sum::<f64>() as u64;
        let mut pilot = Autopilot::new(8, 16, Start { line: Some(8.5 * MB), ..Default::default() }, t0, 0).alone(t0, 0);
        let startup = Duration::from_millis(200);
        let decided = (1..=40).map(|n| n * 50).find(|&ms| {
            pilot.open(t0 + Duration::from_millis(ms), bytes(ms), 1 << 30, Some(startup), None, None);
            !pilot.is_alone()
        });
        assert!(decided.is_some_and(|ms| ms >= 1000), "{decided:?}: {}", pilot.why());
        assert!((pilot.width, pilot.done) == (1, true) && pilot.why().starts_with("1 connection: that fills your line"), "{}", pilot.why());
    }

    /// A stream whose climb stops for a window and then goes on, as one from a far host does (1 MB/s
    /// for two windows, 1.1 for one, then 4; a window as long as a new connection takes to answer):
    /// its speed is read once the fastest it kept over two windows is passed by neither of the two
    /// after it, and that is its speed, here one that fills the line. The first window no faster
    /// than the one before read a quarter of it. A burst of a window (a server's idle credit, at
    /// 8 MB/s) is not a speed it kept.
    #[test]
    fn test_a_speed_is_read_once_it_stopped_climbing() {
        let t0 = Instant::now();
        let line = Start { line: Some(4.2 * MB), ..Default::default() };
        for burst in [false, true] {
            let rate = |ms: u64| MB * if ms < 200 { 1.0 } else if ms < 300 { 1.1 } else if burst && ms < 400 { 8.0 } else { 4.0 };
            let bytes = |ms: u64| (0..ms).map(|t| rate(t) / 1000.0).sum::<f64>() as u64;
            let mut pilot = Autopilot::new(8, 16, line, t0, 0).alone(t0, 0);
            let decided = (1..=20).map(|n| n * 50).find(|&ms| {
                pilot.open(t0 + Duration::from_millis(ms), bytes(ms), 1 << 30, Some(Duration::from_millis(100)), None, None);
                !pilot.is_alone()
            });
            assert_eq!(decided, Some(700), "{}", pilot.why());
            assert!((pilot.width, pilot.done) == (1, true) && pilot.why() == "1 connection: that fills your line (about 3.8 MiB/s)", "{}", pilot.why());
        }
    }

    /// Opens one more beside a stream from a host 100 ms from an answer that went at 4 MB/s alone,
    /// whose answer comes `answer` ms later, the stream going at `before` MB/s meanwhile; then
    /// steps it through windows of 100 ms in which the stream and the new connection go at these
    /// speeds (MB/s): the pilot, and when it decided (ms after the answer) on what width.
    fn judged(answer: u64, before: f64, windows: &[(f64, f64)]) -> (Autopilot, Option<(u64, usize)>) {
        let (t0, startup) = (Instant::now(), Duration::from_millis(100));
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut pilot = Autopilot::new(8, 16, Start::default(), t0, 0).alone(t0, 0);
        let mut ms = (1..=20).map(|n| n * 50).find(|&ms| pilot.open(at(ms), ms * 4_000, 1 << 30, Some(startup), None, None).is_some()).expect("one more");
        let (mut all, answered) = (ms * 4_000, ms + answer);
        while ms < answered {
            ms += 100;
            all += (before * 1e5) as u64;
            pilot.judge(at(ms), all, all, false, startup);
        }
        let mut first = all;
        pilot.judge(at(ms), all, first, true, startup);
        let decided = windows.iter().find_map(|&(stream, newcomer)| {
            ms += 100;
            (all, first) = (all + ((stream + newcomer) * 1e5) as u64, first + (stream * 1e5) as u64);
            pilot.judge(at(ms), all, first, true, startup);
            (!pilot.is_judging()).then_some((ms - answered, pilot.width))
        });
        (pilot, decided)
    }

    /// One more beside the stream is judged by what it brings of its own, once its own speed
    /// stopped climbing (see [`Autopilot::judge`]): all the rest open when the two together go
    /// `PAY` times as fast as the stream alone at its best, or when they do not but the stream kept
    /// that speed beside a new connection slow of its own (one fading out, say), not one that climbs
    /// slowly on a line the stream fills; else the stream alone goes on, and the speed it held is
    /// kept as the host's limit. So too when the stream reaches the new connection's piece first.
    #[test]
    fn test_one_more_opens_the_rest_only_if_it_pays() {
        // A line of 4 MB/s the two share once the new connection's climb is over: a young one takes
        // little yet, which is not slow of its own. Then the stream alone, the line its limit.
        let climb = [(3.95, 0.05), (3.9, 0.1), (3.8, 0.2), (3.6, 0.4), (3.2, 0.8), (2.4, 1.6), (2.0, 2.0), (2.0, 2.0), (2.0, 2.0)];
        let (pilot, decided) = judged(100, 4.0, &climb);
        assert_eq!(decided, Some((900, 1)), "{}", pilot.why());
        assert_eq!((pilot.done, pilot.settled), (true, Some(1)));
        assert!(pilot.limit().is_some_and(|limit| (limit / 4e6 - 1.0).abs() < 0.05), "{:?}", pilot.limit());
        // The stream climbs on to 6 MB/s beside the new connection, on a line of 6: that is the
        // stream's speed alone, not what the new one brought.
        let climb = [(5.0, 0.05), (6.0, 0.1), (5.8, 0.2), (5.6, 0.4), (5.2, 0.8), (4.4, 1.6), (3.0, 3.0), (3.0, 3.0), (3.0, 3.0)];
        let (pilot, decided) = judged(100, 4.0, &climb);
        assert_eq!(decided, Some((900, 1)), "{}", pilot.why());
        assert!(pilot.limit().is_some_and(|limit| (limit / 6e6 - 1.0).abs() < 0.05), "{:?}", pilot.limit());
        // At 6 MB/s alone before the new connection's answer: 7 together is not `PAY` times that.
        let (pilot, decided) = judged(300, 6.0, &[(4.5, 2.5); 3]);
        assert_eq!(decided, Some((300, 1)), "{}", pilot.why());
        // As fast again, three windows in a row: the rest open.
        let (pilot, decided) = judged(100, 4.0, &[(4.0, 4.0); 3]);
        assert_eq!(decided, Some((300, 8)), "{}", pilot.why());
        assert!(!pilot.done && pilot.why().starts_with("8 connections: the first went at 3.8 MiB/s, and a second made it faster"), "{}", pilot.why());
        // The new connection fades out after its start, the stream at its speed: slow of its own.
        let (pilot, decided) = judged(100, 4.0, &[(4.0, 2.5), (4.0, 0.06), (4.0, 0.06)]);
        assert_eq!(decided, Some((300, 8)), "{}", pilot.why());
        assert!(!pilot.done && pilot.limit().is_none(), "{}", pilot.why());
        assert!(pilot.why().starts_with("8 connections: the first kept its 3.8 MiB/s beside a second, which was slow of its own"), "{}", pilot.why());
        // On a line the stream fills, the new connection gets 2 % of it and gains 2 % a second,
        // taken from the stream: the stream keeps over 90 %, but the other climbs, not slow of its
        // own.
        let full: Vec<(f64, f64)> = (0..50).map(|k| 0.08 + 0.008 * k as f64).map(|newcomer| (4.0 - newcomer, newcomer)).collect();
        let (pilot, decided) = judged(100, 4.0, &full);
        assert!(decided.is_some_and(|(_, width)| width == 1), "{decided:?}: {}", pilot.why());
        assert!(pilot.limit().is_some_and(|limit| (limit / 4e6 - 1.0).abs() < 0.05), "{:?}", pilot.limit());
        // The stream reached the new connection's piece before the verdict: the stream alone.
        let (mut pilot, _) = judged(100, 4.0, &full[..3]);
        assert!(pilot.is_judging());
        assert_eq!((pilot.overtaken(), pilot.done, pilot.settled, pilot.is_judging()), (Some(1), true, Some(1), false));
        assert!(pilot.limit().is_some_and(|limit| (limit / 4e6 - 1.0).abs() < 0.05), "{:?}", pilot.limit());
        // Refused while it widened, the host serving two: those, and no more.
        let (mut pilot, _) = judged(100, 4.0, &[]);
        assert_eq!((pilot.took(2), pilot.done, pilot.settled), (None, true, Some(2)));
        assert_eq!(pilot.why(), "2 connections: the server refused more at once (429/503), so it gets no more");
    }

    /// From the host's past: more are still tried, but not fewer; one more that made it slower is
    /// taken back; and too little left tries nothing.
    #[test]
    fn test_a_remembered_width_is_kept_unless_more_pays() {
        let mut model = Model { per_connection: 2.0 * MB, per_client: 4.0 * MB, ..Default::default() };
        let start = Start { remembered: true, limit: Some(4.0 * MB), ..Default::default() };
        let mut pilot = pilot(2, 32, start);
        assert_eq!(pilot.why(), "2 connections: downloads from this site were fastest with that");
        assert_eq!(drive(&mut pilot, &mut model, 6), [2, 3, 2]);

        let t0 = Instant::now();
        let mut pilot = Autopilot::new(2, 32, Start::default(), t0, 0);
        let at = |s: u64| t0 + Duration::from_secs(s);
        let startup = Duration::from_millis(250);
        pilot.next(at(1), 4_000_000, 1 << 40, startup, None);
        assert_eq!(pilot.next(at(2), 8_000_000, 1 << 40, startup, None), Some(3));
        pilot.next(at(3), 9_000_000, 1 << 40, startup, None);
        assert_eq!(pilot.next(at(4), 11_000_000, 1 << 40, startup, None), Some(2), "slower: taken back");
        assert_eq!(pilot.why(), "2 connections: more made it slower");

        let mut pilot = Autopilot::new(2, 32, Start::default(), t0, 0);
        pilot.next(at(1), 4_000_000, 1 << 40, startup, None);
        assert_eq!(pilot.next(at(2), 8_000_000, 10_000_000, startup, None), None, "10 MB left at 4 MB/s");
        assert_eq!((pilot.done, pilot.settled), (true, None));
        assert_eq!(pilot.why(), "2 connections: too little is left to try more");
    }
}
