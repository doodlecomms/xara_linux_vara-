use std::fmt;
use std::io;
use std::time::{Duration, Instant};

use super::ratematch::{self, Event, Interp, RateMatch, STALL, TxBuffer};
use super::{Audio, BLOCK, Path, RATE, TxAudio, quantize, skip_backlog};

const STATUS_EVERY: Duration = Duration::from_secs(60);
const MOVED: usize = RATE as usize / 500;
const MEAN_BLOCKS: f64 = 100.0;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Budget {
    pub capture: usize,
    pub backlog: usize,
    pub lead: usize,
    pub keying: usize,
    pub queue: Option<usize>,
    pub interp: usize,
    pub out: usize,
}

impl Budget {
    pub fn total(&self) -> i64 {
        let queue = self.queue.unwrap_or(0);
        (self.capture + self.backlog + self.lead + self.keying + queue + self.interp + self.out) as i64
    }

    pub fn lag(&self) -> i64 {
        (self.capture + self.backlog) as i64
    }
}

impl fmt::Display for Budget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ms = |n: usize| ms(n as f64);
        write!(f, "TX path {:.0} ms = capture {:.0}", ms(self.total() as usize), ms(self.capture))?;
        write!(f, " + backlog {:.0} + lead {:.0} + keying {:.0}", ms(self.backlog), ms(self.lead), ms(self.keying))?;
        if let Some(q) = self.queue {
            write!(f, " + queue {:.0} + interp {:.1}", ms(q), ms(self.interp))?;
        }
        if self.out > 0 {
            write!(f, " + device {:.0}", ms(self.out))?;
        }
        Ok(())
    }
}

#[derive(Default)]
struct Mean(Option<f64>);

impl Mean {
    fn add(&mut self, n: usize) {
        let n = n as f64;
        self.0 = Some(self.0.map_or(n, |m| m + (n - m) / MEAN_BLOCKS));
    }

    fn get(&self) -> usize {
        self.0.map_or(0, |m| m.round() as usize)
    }
}

pub struct TxPath {
    delay: TxAudio,
    rate: Option<RateMatch>,
    budget: Budget,
    capture: Mean,
    backlog: Mean,
    catching_up: bool,
    prime: usize,
    matched: Vec<f64>,
    played: Vec<i16>,
    last: Instant,
    noted: Instant,
}

impl TxPath {
    pub fn new(lead: usize, buf: TxBuffer, path: Path) -> Self {
        let rate = path.queue.then(|| RateMatch::new(buf));
        let mut capture = Mean::default();
        if let Some(n) = path.capture {
            capture.add(n);
        }
        let budget = Budget {
            capture: capture.get(),
            backlog: 0,
            lead,
            keying: path.keying,
            queue: rate.as_ref().map(RateMatch::latency),
            interp: if rate.is_some() { Interp::DELAY } else { 0 },
            out: path.out,
        };
        let now = Instant::now();
        Self {
            delay: TxAudio::new(lead),
            prime: if rate.is_some() { buf.target } else { 0 },
            rate,
            budget,
            capture,
            backlog: Mean::default(),
            catching_up: false,
            matched: Vec::new(),
            played: Vec::new(),
            last: now,
            noted: now,
        }
    }

    pub fn budget(&self) -> Budget {
        self.budget
    }

    pub fn start(&mut self, io: &mut dyn Audio) -> io::Result<()> {
        skip_backlog(io)?;
        io.write(&vec![0; self.prime])?;
        (self.last, self.noted) = (Instant::now(), Instant::now());
        Ok(())
    }

    pub fn send(
        &mut self,
        io: &mut dyn Audio,
        out: &mut [f64],
        forced: bool,
        waited: Duration,
        backlog: Option<usize>,
    ) -> io::Result<Option<Budget>> {
        self.delay.process(out, forced);
        if Instant::now().duration_since(self.last).saturating_sub(waited) > STALL {
            self.catching_up = true;
        }
        let path = io.path()?;
        if let Some(n) = path.capture {
            self.capture.add(n);
        }
        if let Some(n) = backlog {
            self.catching_up &= n >= BLOCK;
            if !self.catching_up {
                self.backlog.add(n);
            }
        }
        let mut settled = None;
        let now = match &mut self.rate {
            None => {
                self.played.clear();
                self.played.extend(out.iter().map(|&v| quantize(v)));
                io.write(&self.played)?;
                Instant::now()
            }
            Some(rate) => {
                rate.process(out, &mut self.matched);
                self.played.clear();
                self.played.extend(self.matched.iter().map(|&v| quantize(v)));
                io.write(&self.played)?;
                let q = io.queued()?;
                let now = Instant::now();
                for e in rate.steer(q, backlog, now.duration_since(self.last).saturating_sub(waited)) {
                    match e {
                        Event::Latency(n) => settled = Some(n),
                        e => report(e),
                    }
                }
                now
            }
        };
        self.last = now;
        let due = now.duration_since(self.noted) >= STATUS_EVERY;
        let mut changed = None;
        if settled.is_some() || due {
            let b = Budget {
                capture: self.capture.get(),
                backlog: self.backlog.get(),
                keying: path.keying,
                queue: settled.or(self.budget.queue),
                out: path.out,
                ..self.budget
            };
            if settled.is_some() || b.total().abs_diff(self.budget.total()) > MOVED as u64 {
                self.budget = b;
                changed = Some(b);
                crate::log::note(format_args!("audio: {b}"));
            }
        }
        if let (true, Some(rate)) = (due, &self.rate) {
            let s = rate.status();
            crate::log::note(format_args!(
                "audio: TX rate {:+.0} ppm (clock {:+.0} ppm), queue {:.1} ms holding {:.1} ms, {} underruns",
                s.ppm,
                s.skew,
                ms(s.fill),
                ms(s.hold),
                s.underruns
            ));
        }
        if due {
            self.noted = now;
        }
        Ok(changed)
    }
}

fn ms(frames: f64) -> f64 {
    frames * 1e3 / f64::from(RATE)
}

fn report(e: Event) {
    match e {
        Event::Aligned { start, hold, target } => info!(
            "audio: playback latency {:.0} ms at start, holding {:.0} ms for {:.0} ms of headroom",
            ms(start),
            ms(hold),
            ms(target)
        ),
        Event::Realign(n) => {
            info!("audio: TX queue {:+.0} ms off after a stall, realigning while silent", -ms(n as f64))
        }
        Event::Latency(_) => {}
        Event::Skew(ppm) => say!("audio: playback clock {ppm:+.0} ppm against capture, rate matched"),
        Event::Pinned => say!(
            "audio: playback and capture clocks differ by more than {:.0}%, past what rate matching covers",
            ratematch::MAX_SKEW * 100.0
        ),
        Event::Headroom { hold, headroom } => {
            let fix = match ms(headroom).ceil() as u32 {
                n if n <= ratematch::MAX_MS => format!(" (--tx-buffer-ms {n} avoids this)"),
                _ => String::new(),
            };
            say!("audio: TX queue ran dry without a stall, holding {:.0} ms from now on{fix}", ms(hold))
        }
        Event::NoQueue => info!("audio: the playback device reports no queue, so TX is not rate matched"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::ratematch::Queue;
    use crate::audio::{BLOCK, samples};

    #[test]
    fn the_budget_adds_every_term_once() {
        let b =
            Budget { capture: 1, backlog: 600_000, lead: 20, keying: 300, queue: Some(4000), interp: 8, out: 50_000 };
        assert_eq!(b.total(), 654_329);
        assert_eq!(Budget { queue: None, ..b }.total(), 650_329);
        let b = Budget { capture: 480, backlog: 240, lead: 0, keying: 2880, queue: Some(1920), interp: 8, out: 144 };
        assert_eq!(
            b.to_string(),
            "TX path 118 ms = capture 10 + backlog 5 + lead 0 + keying 60 + queue 40 + interp 0.2 + device 3"
        );
        assert_eq!(
            Budget { queue: None, interp: 0, ..b }.to_string(),
            "TX path 78 ms = capture 10 + backlog 5 + lead 0 + keying 60 + device 3"
        );
        assert_eq!(
            Budget { queue: None, interp: 0, out: 0, ..b }.to_string(),
            "TX path 75 ms = capture 10 + backlog 5 + lead 0 + keying 60"
        );
    }

    struct Fixed {
        path: Path,
        written: Vec<i16>,
        queued: usize,
    }

    impl Audio for Fixed {
        fn read(&mut self, x: &mut [i16]) -> io::Result<()> {
            x.fill(0);
            Ok(())
        }

        fn write(&mut self, x: &[i16]) -> io::Result<()> {
            self.written.extend_from_slice(x);
            Ok(())
        }

        fn queued(&mut self) -> io::Result<Option<Queue>> {
            self.queued += 1;
            Ok(Some(Queue { frames: 1920, floor: 0, underruns: 0 }))
        }

        fn backlog(&mut self) -> io::Result<Option<usize>> {
            Ok(Some(0))
        }

        fn path(&mut self) -> io::Result<Path> {
            Ok(self.path)
        }
    }

    #[test]
    fn a_backend_without_a_queue_gets_no_rate_matcher() {
        let lead = samples(Duration::from_millis(5));
        let path = Path { capture: Some(100), keying: 2880, out: 0, queue: false };
        let mut io = Fixed { path, written: Vec::new(), queued: 0 };
        let mut tx = TxPath::new(lead, TxBuffer::ms(100), io.path().unwrap());
        assert!(tx.rate.is_none());
        assert_eq!(
            tx.budget(),
            Budget { capture: 100, backlog: 0, lead, keying: 2880, queue: None, interp: 0, out: 0 }
        );
        assert_eq!(tx.budget().total(), (100 + lead + 2880) as i64);
        tx.start(&mut io).unwrap();
        assert!(io.written.is_empty());
        let mut out = vec![0.5; BLOCK];
        assert_eq!(tx.send(&mut io, &mut out, false, Duration::ZERO, Some(0)).unwrap(), None);
        assert_eq!((io.written.len(), io.queued), (BLOCK, 0));
        assert!(io.written[..lead].iter().all(|&v| v == 0) && io.written[lead..].iter().all(|&v| v == quantize(0.5)));
    }

    #[test]
    fn a_queued_backend_holds_the_target_and_counts_the_interpolator() {
        let path = Path { capture: None, keying: 0, out: 144, queue: true };
        let mut io = Fixed { path, written: Vec::new(), queued: 0 };
        let mut tx = TxPath::new(0, TxBuffer::ms(40), io.path().unwrap());
        assert_eq!(
            tx.budget(),
            Budget { capture: 0, backlog: 0, lead: 0, keying: 0, queue: Some(1920), interp: Interp::DELAY, out: 144 }
        );
        tx.start(&mut io).unwrap();
        assert_eq!(io.written.len(), 1920);
        tx.send(&mut io, &mut [0.0; BLOCK], false, Duration::ZERO, Some(0)).unwrap();
        assert_eq!(io.queued, 1);
    }

    #[test]
    fn a_mean_averages_out_a_fragment_sawtooth() {
        let mut m = Mean::default();
        assert_eq!(m.get(), 0);
        m.add(480);
        assert_eq!(m.get(), 480);
        for i in 0..2000 {
            m.add(i * 137 % 480);
        }
        assert!(m.get().abs_diff(240) <= 24, "{}", m.get());
    }

    #[test]
    fn chunked_capture_counts_its_backlog_but_stall_catch_up_does_not() {
        let path = Path { capture: None, keying: 0, out: 0, queue: false };
        let mut io = Fixed { path, written: Vec::new(), queued: 0 };
        let mut tx = TxPath::new(0, TxBuffer::default(), io.path().unwrap());
        let mut send =
            |tx: &mut TxPath, n| tx.send(&mut io, &mut [0.0; BLOCK], false, Duration::ZERO, Some(n)).unwrap();
        for k in 0..2000 {
            send(&mut tx, [544, 64][k % 2]);
        }
        assert!(tx.backlog.get().abs_diff(304) <= 5, "{}", tx.backlog.get());
        tx.last -= 4 * STALL;
        for k in (1..=10).rev() {
            send(&mut tx, k * BLOCK);
        }
        send(&mut tx, 64);
        assert!(tx.backlog.get().abs_diff(304) <= 5, "{}", tx.backlog.get());
    }

    #[test]
    fn small_moves_add_up_against_what_the_station_was_told() {
        let path = Path { capture: None, keying: 0, out: 0, queue: false };
        let mut io = Fixed { path, written: Vec::new(), queued: 0 };
        let mut tx = TxPath::new(0, TxBuffer::default(), io.path().unwrap());
        let settle = |io: &mut Fixed, tx: &mut TxPath, capture| {
            io.path.capture = Some(capture);
            for _ in 0..2000 {
                tx.send(io, &mut [0.0; BLOCK], false, Duration::ZERO, Some(0)).unwrap();
            }
            tx.noted -= STATUS_EVERY;
            tx.send(io, &mut [0.0; BLOCK], false, Duration::ZERO, Some(0)).unwrap().map(|b| b.capture)
        };
        assert_eq!(settle(&mut io, &mut tx, 200), Some(200));
        assert_eq!(settle(&mut io, &mut tx, 250), None);
        assert_eq!(tx.budget().capture, 200);
        assert_eq!(settle(&mut io, &mut tx, 300), Some(300));
    }
}
