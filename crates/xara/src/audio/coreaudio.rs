use std::collections::VecDeque;
use std::io;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use super::coreaudio_ffi::{
    self as ffi, AudioQueue, Capture, DeviceInfo, Playback, StreamFormat, TRANSPORT_AGGREGATE, TRANSPORT_VIRTUAL,
};
use super::ratematch::{Queue, TxBuffer};
use super::{Audio, BLOCK, DeviceList, IO_TIMEOUT, Path, RATE, Report, capture_only, remaining};
use crate::lock;

const RX_BUFFERS: usize = 16;
const TX_BUFFERS: usize = 3;
const RX_LIMIT: usize = 2 * RATE as usize;
const SETUP: Duration = Duration::from_secs(5);
const STALL: Duration = Duration::from_secs(3);

#[derive(Default)]
struct State {
    rx: VecDeque<i16>,
    tx: VecDeque<i16>,
    inflight: usize,
    heard: bool,
    played: bool,
    reading: bool,
    writing: bool,
    overruns: u64,
    underruns: u64,
    rx_lost: usize,
    tx_lost: usize,
}

struct Shared {
    state: Mutex<State>,
    moved: Condvar,
}

impl Shared {
    fn new() -> Self {
        let rx = VecDeque::with_capacity(RX_LIMIT + BLOCK);
        Self { state: Mutex::new(State { rx, ..State::default() }), moved: Condvar::new() }
    }

    fn wait<'a>(&self, st: MutexGuard<'a, State>, until: Instant, what: &str) -> io::Result<MutexGuard<'a, State>> {
        let left = remaining(until, what)?;
        Ok(self.moved.wait_timeout(st, left).unwrap_or_else(PoisonError::into_inner).0)
    }
}

impl Capture for Shared {
    fn captured(&self, x: &[i16]) {
        let mut st = lock(&self.state);
        st.heard = true;
        st.rx.extend(x);
        let over = st.rx.len().saturating_sub(RX_LIMIT);
        if over > 0 {
            st.rx.drain(..over);
            st.overruns += u64::from(st.reading);
        }
        drop(st);
        self.moved.notify_all();
    }

    fn rx_lost(&self) {
        lock(&self.state).rx_lost += 1;
        self.moved.notify_all();
    }
}

impl Playback for Shared {
    fn refill(&self, played: usize, out: &mut [i16]) {
        let mut st = lock(&self.state);
        let n = st.tx.len().min(out.len());
        for (o, v) in out.iter_mut().zip(st.tx.drain(..n)) {
            *o = v;
        }
        out[n..].fill(0);
        st.underruns += u64::from(n < out.len() && st.writing);
        st.inflight = st.inflight.saturating_sub(played) + out.len();
        st.played |= played > 0;
        drop(st);
        self.moved.notify_all();
    }

    fn tx_lost(&self, frames: usize) {
        let mut st = lock(&self.state);
        st.inflight = st.inflight.saturating_sub(frames);
        st.tx_lost += 1;
        drop(st);
        self.moved.notify_all();
    }
}

fn tx_buffers(d: &DeviceInfo) -> usize {
    TX_BUFFERS.max(scaled(d, 0).div_ceil(BLOCK) + 1)
}

fn scaled(d: &DeviceInfo, frames: usize) -> usize {
    let rate = if d.rate > 0.0 { d.rate } else { f64::from(RATE) };
    ((d.io_frames + frames) as f64 * f64::from(RATE) / rate).round() as usize
}

fn dead(what: &str, name: &str, why: &str) -> io::Error {
    io::Error::other(format!("coreaudio {what} {name:?}: {why}"))
}

struct Opening {
    _done: Sender<()>,
}

impl Opening {
    fn watch() -> Self {
        let (done, rx) = mpsc::channel();
        let hint = move || {
            if rx.recv_timeout(STALL) == Err(RecvTimeoutError::Timeout) {
                say!("coreaudio: still opening devices; if macOS is asking for microphone access, click Allow");
            }
        };
        let _ = thread::Builder::new().name("coreaudio-open".into()).spawn(hint);
        Self { _done: done }
    }
}

fn on<'a>(what: &'a str, name: &'a str) -> impl Fn(io::Error) -> io::Error + 'a {
    move |e| io::Error::other(format!("{what} {name:?}: {e}"))
}

fn is_default(name: &str) -> bool {
    name.is_empty() || name == "default"
}

fn find<'a>(devs: &'a [DeviceInfo], name: &str, input: bool) -> io::Result<&'a DeviceInfo> {
    let what = if input { "capture" } else { "playback" };
    let fits = |d: &&DeviceInfo| if input { d.inputs > 0 } else { d.outputs > 0 };
    if let Some(d) = devs.iter().filter(fits).find(|d| d.uid == name) {
        return Ok(d);
    }
    let named: Vec<_> = devs.iter().filter(fits).filter(|d| d.name == name).collect();
    match named[..] {
        [d] => Ok(d),
        [] => Err(io::Error::other(format!("no {what} device named {name:?}"))),
        _ => {
            let uids: Vec<_> = named.iter().map(|d| d.uid.as_str()).collect();
            Err(io::Error::other(format!("{what} name {name:?} is ambiguous; use a device UID: {}", uids.join(", "))))
        }
    }
}

fn check_playback<'a>(devs: &'a [DeviceInfo], name: &str, radio: bool) -> io::Result<&'a DeviceInfo> {
    if is_default(name) {
        return Err(io::Error::other(match radio {
            true => "--radio needs the playback device named explicitly, not a default",
            false => "refusing playback to the default output: use a virtual device such as BlackHole",
        }));
    }
    let d = find(devs, name, false)?;
    if !radio && d.transport != TRANSPORT_VIRTUAL {
        return Err(io::Error::other(format!(
            "refusing playback to {name:?}: not a virtual audio device such as BlackHole"
        )));
    }
    Ok(d)
}

pub fn list() -> io::Result<(DeviceList, DeviceList)> {
    let devs = ffi::devices()?;
    let desc = |d: &DeviceInfo| match d.transport {
        TRANSPORT_VIRTUAL => format!("{}, virtual", d.name),
        TRANSPORT_AGGREGATE => format!("{}, aggregate", d.name),
        _ => d.name.clone(),
    };
    let pick = |input: bool| {
        devs.iter()
            .filter(|d| if input { d.inputs > 0 } else { d.outputs > 0 })
            .map(|d| (d.uid.clone(), desc(d)))
            .collect()
    };
    Ok((pick(true), pick(false)))
}

fn default_input(devs: &[DeviceInfo]) -> io::Result<&DeviceInfo> {
    let id = ffi::default_input()?;
    let d = devs
        .iter()
        .find(|d| d.id == id && d.inputs > 0)
        .ok_or_else(|| io::Error::other("no default capture device"))?;
    info!("coreaudio capture default: {:?} ({})", d.name, d.uid);
    Ok(d)
}

pub fn check(playback: &str, radio: bool) -> io::Result<()> {
    check_playback(&ffi::devices()?, playback, radio).map(drop)
}

pub struct CoreAudioIo {
    shared: Arc<Shared>,
    _rec: AudioQueue<Shared>,
    rec_id: u32,
    rec_latency: usize,
    capture: String,
    overruns: Report,
    play: Option<Play>,
}

struct Play {
    q: AudioQueue<Shared>,
    id: u32,
    buffers: usize,
    latency: usize,
    name: String,
    buf: TxBuffer,
    underruns: Report,
}

impl CoreAudioIo {
    pub fn open(capture: &str, playback: &str, radio: bool, tx: TxBuffer) -> io::Result<Self> {
        Self::connect(capture, Some((playback, radio, tx)))
    }

    pub fn open_capture(capture: &str) -> io::Result<Self> {
        Self::connect(capture, None)
    }

    fn connect(capture: &str, playback: Option<(&str, bool, TxBuffer)>) -> io::Result<Self> {
        let devs = ffi::devices()?;
        let rec_dev = if is_default(capture) { default_input(&devs)? } else { find(&devs, capture, true)? };
        if rec_dev.inputs > 2 {
            info!("coreaudio capture {capture:?}: {} channels, using channel 1", rec_dev.inputs);
        }
        let playback = playback
            .map(|(name, radio, buf)| check_playback(&devs, name, radio).map(|d| (name, d, buf)))
            .transpose()?;
        let _opening = Opening::watch();
        let shared = Arc::new(Shared::new());
        let fmt = StreamFormat::s16_mono(RATE);
        let rec =
            AudioQueue::input(&fmt, rec_dev, shared.clone(), RX_BUFFERS, BLOCK).map_err(on("capture", capture))?;
        let play = playback
            .map(|(name, d, buf)| {
                let (n, latency) = (tx_buffers(d), scaled(d, d.out_latency));
                info!(
                    "coreaudio playback {name:?}: device IO {} frames, {n} buffers of {BLOCK} frames, \
                     device latency {latency} frames",
                    scaled(d, 0)
                );
                if d.outputs > 2 {
                    info!("coreaudio playback {name:?}: {} channels, using channel 1", d.outputs);
                }
                Ok::<_, io::Error>(Play {
                    q: AudioQueue::output(&fmt, d, shared.clone(), n, BLOCK).map_err(on("playback", name))?,
                    id: d.id,
                    buffers: n,
                    latency,
                    name: name.into(),
                    buf,
                    underruns: Report::default(),
                })
            })
            .transpose()?;
        rec.start().map_err(on("capture", capture))?;
        if let Some(p) = &play {
            p.q.start().map_err(on("playback", &p.name))?;
        }
        let until = Instant::now() + SETUP;
        let mut st = lock(&shared.state);
        while !(st.heard && (play.is_none() || st.played)) {
            let what = match &play {
                Some(p) if st.heard => format!("starting playback {:?}", p.name),
                _ => format!("starting capture {capture:?}"),
            };
            st = shared.wait(st, until, &what)?;
        }
        drop(st);
        Ok(Self {
            shared,
            _rec: rec,
            rec_id: rec_dev.id,
            rec_latency: scaled(rec_dev, rec_dev.in_latency) + BLOCK,
            capture: capture.into(),
            overruns: Report::default(),
            play,
        })
    }
}

impl Audio for CoreAudioIo {
    fn read(&mut self, x: &mut [i16]) -> io::Result<()> {
        if !ffi::alive(self.rec_id) {
            return Err(dead("capture", &self.capture, "device is gone"));
        }
        let (n, until) = (x.len(), Instant::now() + IO_TIMEOUT);
        let mut st = lock(&self.shared.state);
        st.reading = true;
        while st.rx.len() < n {
            if st.rx_lost >= RX_BUFFERS {
                return Err(dead("capture", &self.capture, "AudioQueue lost all its buffers"));
            }
            st = self.shared.wait(st, until, "capture (no audio)")?;
        }
        for (v, s) in x.iter_mut().zip(st.rx.drain(..n)) {
            *v = s;
        }
        let overruns = st.overruns;
        drop(st);
        if self.overruns.due(overruns) {
            say!("coreaudio capture {:?}: overrun, dropped audio ({overruns} so far)", self.capture);
        }
        Ok(())
    }

    fn write(&mut self, x: &[i16]) -> io::Result<()> {
        let Some(play) = &self.play else { return Err(capture_only()) };
        let until = Instant::now() + IO_TIMEOUT;
        let mut st = lock(&self.shared.state);
        while st.tx.len() + x.len() > play.buf.capacity() {
            if st.tx_lost >= play.buffers {
                return Err(dead("playback", &play.name, "AudioQueue lost all its buffers"));
            }
            st = self.shared.wait(st, until, "playback (device not draining)")?;
        }
        st.tx.extend(x);
        st.writing = true;
        Ok(())
    }

    fn queued(&mut self) -> io::Result<Option<Queue>> {
        let Some(play) = &mut self.play else { return Err(capture_only()) };
        if !ffi::alive(play.id) {
            return Err(dead("playback", &play.name, "device is gone"));
        }
        let st = lock(&self.shared.state);
        if st.tx_lost >= play.buffers {
            return Err(dead("playback", &play.name, "AudioQueue lost all its buffers"));
        }
        let q = Queue { frames: st.tx.len() + st.inflight, floor: st.inflight, underruns: st.underruns };
        drop(st);
        if play.underruns.due(q.underruns) {
            say!("coreaudio playback {:?}: underrun ({} so far)", play.name, q.underruns);
        }
        Ok(Some(q))
    }

    fn backlog(&mut self) -> io::Result<Option<usize>> {
        Ok(Some(lock(&self.shared.state).rx.len()))
    }

    fn path(&mut self) -> io::Result<Path> {
        let out = self.play.as_ref().map_or(0, |p| p.latency);
        Ok(Path { capture: Some(self.rec_latency), out, queue: true, ..Path::default() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(uid: &str, name: &str, outputs: bool, transport: u32) -> DeviceInfo {
        let (i, o) = if outputs { (0, 2) } else { (2, 0) };
        DeviceInfo {
            id: 0,
            uid: uid.into(),
            name: name.into(),
            inputs: i,
            outputs: o,
            transport,
            io_frames: 512,
            in_latency: 0,
            out_latency: 0,
            rate: 48000.0,
        }
    }

    #[test]
    fn playback_device_checks() {
        let usb = u32::from_be_bytes(*b"usb ");
        let devs = [
            dev("BlackHole2ch_UID", "BlackHole 2ch", true, TRANSPORT_VIRTUAL),
            dev("AppleUSBAudioEngine:1", "USB Audio CODEC", true, usb),
            dev("AppleUSBAudioEngine:2", "USB Audio CODEC", true, usb),
            dev("AppleUSBAudioEngine:3", "Mic", false, usb),
        ];
        let err = |name: &str, radio: bool| check_playback(&devs, name, radio).err().map(|e| e.to_string());
        for name in ["default", ""] {
            assert!(err(name, true).is_some_and(|e| e.contains("named explicitly")), "{name}");
            assert!(err(name, false).is_some_and(|e| e.contains("default output")), "{name}");
        }
        assert!(err("nothing", true).is_some_and(|e| e.contains("no playback device")));
        assert!(err("Mic", true).is_some_and(|e| e.contains("no playback device")));
        assert!(err("USB Audio CODEC", true).is_some_and(|e| e.contains("ambiguous")));
        assert!(err("AppleUSBAudioEngine:1", false).is_some_and(|e| e.contains("not a virtual audio device")));
        assert_eq!(
            (err("AppleUSBAudioEngine:2", true), err("BlackHole 2ch", false), err("BlackHole2ch_UID", true)),
            (None, None, None)
        );
        assert_eq!(find(&devs, "Mic", true).unwrap().uid, "AppleUSBAudioEngine:3");
    }

    #[test]
    fn playback_buffers_cover_a_device_cycle_with_one_to_spare() {
        let mut d = dev("x", "x", true, TRANSPORT_VIRTUAL);
        let mut n = |io, rate| {
            (d.io_frames, d.rate) = (io, rate);
            tx_buffers(&d)
        };
        let at48: Vec<_> = [0, 256, 480, 512, 960, 1024, 4096].into_iter().map(|io| n(io, 48000.0)).collect();
        assert_eq!(at48, [3, 3, 3, 3, 3, 4, 10]);
        assert_eq!(n(960, 44100.0), 4);
    }

    #[test]
    fn device_latency_is_scaled_to_48_khz() {
        let mut d = dev("x", "x", true, TRANSPORT_VIRTUAL);
        assert_eq!(scaled(&d, 88), 600);
        (d.io_frames, d.rate) = (512, 44100.0);
        assert_eq!(scaled(&d, 32), 592);
        d.rate = 0.0;
        assert_eq!(scaled(&d, 32), 544);
    }

    #[test]
    fn playback_pads_after_the_first_write_count_as_underruns() {
        let sh = Shared::new();
        let mut out = [1; BLOCK];
        for _ in 0..TX_BUFFERS {
            sh.refill(0, &mut out);
        }
        assert!(!lock(&sh.state).played);
        sh.refill(BLOCK, &mut out);
        let at = |sh: &Shared| {
            let st = lock(&sh.state);
            (st.tx.len(), st.inflight, st.underruns, st.played)
        };
        assert_eq!((at(&sh), out.iter().all(|&v| v == 0)), ((0, 1440, 0, true), true));
        {
            let mut st = lock(&sh.state);
            st.writing = true;
            st.tx.extend([7; BLOCK + 100]);
        }
        sh.refill(BLOCK, &mut out);
        assert_eq!((at(&sh), out[0]), ((100, 1440, 0, true), 7));
        sh.refill(BLOCK, &mut out);
        assert_eq!((at(&sh), out[99], out[100]), ((0, 1440, 1, true), 7, 0));
        sh.tx_lost(BLOCK);
        assert_eq!(at(&sh), (0, 960, 1, true));
    }

    #[test]
    fn capture_keeps_the_newest_two_seconds() {
        let sh = Shared::new();
        let block: Vec<i16> = (0..BLOCK as i16).collect();
        let at = |sh: &Shared| {
            let st = lock(&sh.state);
            (st.rx.len(), st.overruns, st.heard, st.rx[0])
        };
        for _ in 0..RX_LIMIT / BLOCK + 2 {
            sh.captured(&block);
        }
        assert_eq!(at(&sh), (RX_LIMIT, 0, true, 0));
        lock(&sh.state).reading = true;
        sh.captured(&block[..100]);
        assert_eq!(at(&sh), (RX_LIMIT, 1, true, 100));
    }
}
