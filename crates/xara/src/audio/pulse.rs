use std::io;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::pulse_ffi::{
    BufferAttr, Conn, Peek, RAW_TIMING, S16LE, STREAM_ADJUST_LATENCY, STREAM_AUTO_TIMING_UPDATE, STREAM_DONT_MOVE,
    STREAM_INTERPOLATE_TIMING, SampleSpec, Stream,
};
use super::ratematch::{Queue, TxBuffer};
use super::{Audio, BLOCK, DeviceList, IO_TIMEOUT, Path, RATE, Report, capture_only};

const SETUP: Duration = Duration::from_secs(5);
const SPEC: SampleSpec = SampleSpec { format: S16LE, rate: RATE, channels: 1 };

fn connect() -> io::Result<Conn> {
    Conn::open(Instant::now() + SETUP)
}

struct Sink {
    name: String,
    null: bool,
}

fn sinks(c: &Conn) -> io::Result<Vec<Sink>> {
    let what = "listing playback sinks";
    let mut v = Vec::new();
    let op = c
        .sinks(|s| {
            v.push(Sink {
                name: s.name().unwrap_or_default(),
                null: s.prop(c"factory.name").as_deref() == Some("support.null-audio-sink")
                    && s.has_prop(c"device.api") == Some(false),
            })
        })
        .ok_or_else(|| c.error(what))?;
    let until = Instant::now() + SETUP;
    while op.running() {
        c.iterate(until, what)?;
    }
    if op.failed() {
        return Err(c.error(what));
    }
    drop(op);
    Ok(v)
}

pub fn list() -> io::Result<(DeviceList, DeviceList)> {
    let what = "listing devices";
    let c = connect()?;
    let (mut src, mut snk): (DeviceList, DeviceList) = (Vec::new(), Vec::new());
    let found = |v: &mut DeviceList, name: Option<String>, desc: Option<String>| {
        if let Some(name) = name {
            v.push((name, desc.unwrap_or_default()));
        }
    };
    let sources = c.sources(|i| found(&mut src, i.name(), i.description())).ok_or_else(|| c.error(what))?;
    let sinks = c.sinks(|i| found(&mut snk, i.name(), i.description())).ok_or_else(|| c.error(what))?;
    let until = Instant::now() + SETUP;
    while sources.running() || sinks.running() {
        c.iterate(until, what)?;
    }
    if sources.failed() || sinks.failed() {
        return Err(c.error(what));
    }
    drop((sources, sinks));
    Ok((src, snk))
}

pub fn check(playback: &str, radio: bool) -> io::Result<()> {
    refuse_default(playback, radio)?;
    let c = connect()?;
    check_playback(&sinks(&c)?, playback, radio)
}

fn refuse_default(name: &str, radio: bool) -> io::Result<()> {
    if radio && (name.is_empty() || name.starts_with('@') || name == "default") {
        return Err(io::Error::other("--radio needs the playback sink named explicitly, not a default"));
    }
    Ok(())
}

fn check_playback(sinks: &[Sink], name: &str, radio: bool) -> io::Result<()> {
    refuse_default(name, radio)?;
    if radio {
        if !sinks.iter().any(|s| s.name == name) {
            return Err(io::Error::other(format!("no playback sink named {name:?} (pactl list short sinks)")));
        }
    } else if !sinks.iter().any(|s| s.name == name && s.null) {
        return Err(io::Error::other(format!(
            "refusing playback to {name:?}: not a PipeWire null sink (a real radio interface needs --radio)"
        )));
    }
    Ok(())
}

fn bytes(ms: u32) -> u32 {
    RATE / 1000 * ms * 2
}

fn frame_bytes(frames: usize) -> u32 {
    2 * frames as u32
}

pub struct PulseIo {
    rec: Stream,
    play: Option<Play>,
    conn: Conn,
    rx: Vec<u8>,
    tx: Vec<u8>,
}

struct Play {
    s: Stream,
    name: String,
    buf: TxBuffer,
    report: Report,
    ceiling: usize,
    asked: bool,
}

impl Play {
    fn queue(&self) -> Option<Queue> {
        let Some(t) = self.s.timing() else {
            return Some(Queue { frames: frames(self.s.latency_us()?), floor: 0, underruns: self.report.seen });
        };
        let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?;
        let age = if t.playing { frames(now.saturating_sub(t.at).as_micros() as u64) } else { 0 };
        let floor = frames(t.sink_us);
        let ours = usize::try_from(t.ahead / 2).unwrap_or(0).saturating_sub(age);
        Some(Queue { frames: ours + floor, floor, underruns: self.report.seen })
    }

    fn full(&mut self, n: usize) -> Option<&'static str> {
        let reading = self.queue();
        if let Some(q) = reading {
            (self.ceiling, self.asked) = (q.ours(), false);
        }
        if self.ceiling + n <= self.buf.capacity() {
            return None;
        }
        if reading.is_some() {
            return Some("playback (sink not draining)");
        }
        if !std::mem::replace(&mut self.asked, true) {
            self.s.request_timing();
        }
        Some("playback (no latency reading)")
    }
}

impl PulseIo {
    pub fn open(capture: &str, playback: &str, radio: bool, tx: TxBuffer) -> io::Result<Self> {
        Self::connect(capture, Some((playback, radio, tx)))
    }

    pub fn open_capture(capture: &str) -> io::Result<Self> {
        Self::connect(capture, None)
    }

    fn connect(capture: &str, playback: Option<(&str, bool, TxBuffer)>) -> io::Result<Self> {
        if let Some((name, radio, _)) = playback {
            refuse_default(name, radio)?;
        }
        let conn = connect()?;
        if let Some((name, radio, _)) = playback {
            check_playback(&sinks(&conn)?, name, radio)?;
        }
        let flags = STREAM_DONT_MOVE | STREAM_ADJUST_LATENCY;
        let attr = BufferAttr {
            maxlength: u32::MAX,
            tlength: u32::MAX,
            prebuf: u32::MAX,
            minreq: u32::MAX,
            fragsize: bytes(10),
        };
        let mut rec = Stream::new(&conn, c"rx", &SPEC).ok_or_else(|| io::Error::other("pulse stream"))?;
        rec.connect_record(capture, &attr, flags | STREAM_INTERPOLATE_TIMING | STREAM_AUTO_TIMING_UPDATE)?;
        let play = match playback {
            Some((name, _, buf)) => {
                let mut p = Stream::new(&conn, c"tx", &SPEC).ok_or_else(|| io::Error::other("pulse stream"))?;
                let attr = BufferAttr {
                    maxlength: u32::MAX,
                    tlength: frame_bytes(2 * BLOCK),
                    prebuf: frame_bytes(2 * BLOCK),
                    minreq: u32::MAX,
                    fragsize: u32::MAX,
                };
                let timing = if RAW_TIMING { 0 } else { STREAM_INTERPOLATE_TIMING | STREAM_AUTO_TIMING_UPDATE };
                p.connect_playback(name, &attr, flags | timing)?;
                p.count_underflows();
                Some(Play { s: p, name: name.into(), buf, report: Report::default(), ceiling: 0, asked: false })
            }
            None => None,
        };
        let until = Instant::now() + SETUP;
        while !(rec.live(capture)? & play.as_ref().map_or(Ok(true), |p| p.s.live(&p.name))?) {
            conn.iterate(until, "opening audio streams")?;
        }
        for (s, want) in [(&rec, capture)].into_iter().chain(play.as_ref().map(|p| (&p.s, p.name.as_str()))) {
            let got = s.device_name().unwrap_or_default();
            if got != want {
                return Err(io::Error::other(format!("{want}: the sound server attached {got:?} instead")));
            }
        }
        Ok(Self { rec, play, conn, rx: Vec::new(), tx: Vec::new() })
    }
}

impl Audio for PulseIo {
    fn read(&mut self, x: &mut [i16]) -> io::Result<()> {
        let need = 2 * x.len();
        let until = Instant::now() + IO_TIMEOUT;
        while self.rx.len() < need {
            self.rec.live("capture")?;
            match self.rec.peek("capture")? {
                Peek::Empty => {
                    self.conn.iterate(until, "capture (no audio)")?;
                    continue;
                }
                Peek::Hole(n) => self.rx.resize(self.rx.len() + n, 0),
                Peek::Data(d) => self.rx.extend_from_slice(d),
            }
            self.rec.discard("capture")?;
        }
        for (v, c) in x.iter_mut().zip(self.rx.chunks_exact(2)) {
            *v = i16::from_le_bytes([c[0], c[1]]);
        }
        self.rx.drain(..need);
        Ok(())
    }

    fn write(&mut self, x: &[i16]) -> io::Result<()> {
        let Some(play) = &mut self.play else { return Err(capture_only()) };
        self.tx.clear();
        self.tx.extend(x.iter().flat_map(|v| v.to_le_bytes()));
        let until = Instant::now() + IO_TIMEOUT;
        while let Some(what) = play.full(x.len()) {
            play.s.live("playback")?;
            self.conn.iterate(until, what)?;
        }
        play.s.live("playback")?;
        play.s.write(&self.tx, "playback")?;
        play.ceiling += x.len();
        Ok(())
    }

    fn queued(&mut self) -> io::Result<Option<Queue>> {
        let Some(play) = &mut self.play else { return Err(capture_only()) };
        self.conn.dispatch("playback")?;
        play.s.live("playback")?;
        let n = play.s.underflows();
        if play.report.due(n) {
            say!("pulse playback {:?}: underrun ({n} so far)", play.name);
        }
        let q = play.queue();
        if RAW_TIMING {
            play.s.request_timing();
        }
        Ok(q)
    }

    fn backlog(&mut self) -> io::Result<Option<usize>> {
        self.conn.dispatch("capture")?;
        self.rec.live("capture")?;
        Ok(self.rec.readable().map(|n| (n + self.rx.len()) / 2))
    }

    fn path(&mut self) -> io::Result<Path> {
        let us = self.rec.latency_us();
        let capture = us.map(|us| frames(us).saturating_sub(self.rec.readable().unwrap_or(0) / 2));
        Ok(Path { capture, queue: true, ..Path::default() })
    }
}

fn frames(us: u64) -> usize {
    (us * u64::from(RATE) / 1_000_000) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playback_sink_checks() {
        let sinks = [Sink { name: "xara-null".into(), null: true }, Sink { name: "usb-radio".into(), null: false }];
        let err = |name: &str, radio: bool| check_playback(&sinks, name, radio).err().map(|e| e.to_string());
        for name in ["@DEFAULT_SINK@", "default", ""] {
            assert!(err(name, true).is_some_and(|e| e.contains("named explicitly")), "{name}");
        }
        assert!(err("xara-no-such-sink", true).is_some_and(|e| e.contains("no playback sink")));
        assert!(err("xara-no-such-sink", false).is_some_and(|e| e.contains("not a PipeWire null sink")));
        assert!(err("usb-radio", false).is_some_and(|e| e.contains("not a PipeWire null sink")));
        assert_eq!((err("usb-radio", true), err("xara-null", false), err("xara-null", true)), (None, None, None));
    }
}
