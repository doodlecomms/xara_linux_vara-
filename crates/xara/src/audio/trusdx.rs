use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::ratematch::Queue;
use super::{Audio, DeviceList, Path, RATE, capture_only, quantize, sample};
use crate::lock;
use crate::ptt::{Ptt, modem, open_tty};
use crate::resample::Resampler;

const RX_RATE: u32 = 7825;
const TX_RATE: u32 = 11520;
const SILENCE: u8 = 0x80;
const CAT_MAX: usize = 64;
const KEEP: usize = 2 * RATE as usize;
const KEEP_AFTER_DROP: usize = RATE as usize / 10;
const JITTER: Duration = Duration::from_millis(60);
const STREAMING: Duration = Duration::from_millis(300);
const RESTART: Duration = Duration::from_secs(2);
const STREAM_IDLE: Duration = Duration::from_millis(40);
const KEY_WAIT: Duration = Duration::from_millis(150);
const KEYING: Duration = Duration::from_millis(60);
const SETTLE: Duration = Duration::from_millis(10);
const RECHECK: Duration = Duration::from_millis(100);
const UNKEY_WAIT: Duration = Duration::from_millis(300);

fn level(b: u8) -> f64 {
    (f64::from(b) - f64::from(SILENCE)) / 128.0
}

struct Decoder {
    piece: Vec<u8>,
    audio: bool,
    resampler: Resampler,
    errors: u32,
}

impl Decoder {
    fn new() -> Self {
        let resampler = Resampler::new(f64::from(RX_RATE), f64::from(RATE)).expect("truSDX receive rate");
        Self { piece: Vec::new(), audio: false, resampler, errors: 0 }
    }

    fn receive(&mut self, bytes: &[u8], pcm: &mut VecDeque<i16>) -> bool {
        let mut audio = Vec::new();
        for &b in bytes {
            match b {
                b';' => {
                    if !self.audio && self.piece.trim_ascii() == b"E" {
                        self.errors += 1;
                    }
                    self.audio = false;
                    self.piece.clear();
                }
                _ if self.audio => audio.push(level(b)),
                _ => {
                    self.piece.push(b);
                    if self.piece.trim_ascii_start() == b"US" {
                        self.audio = true;
                        self.piece.clear();
                    } else if self.piece.len() > CAT_MAX {
                        self.audio = true;
                        audio.extend(self.piece.drain(..).map(level));
                    }
                }
            }
        }
        pcm.extend(self.resampler.process(&audio).into_iter().map(quantize));
        !audio.is_empty()
    }
}

struct Encoder(Resampler);

impl Encoder {
    fn new() -> Self {
        Self(Resampler::new(f64::from(RATE), f64::from(TX_RATE)).expect("truSDX transmit rate"))
    }

    fn encode(&mut self, pcm: impl Iterator<Item = i16>, out: &mut Vec<u8>) {
        let x: Vec<f64> = pcm.map(sample).collect();
        out.extend(self.0.process(&x).into_iter().map(byte));
    }
}

fn byte(v: f64) -> u8 {
    match ((v * 128.0).round().clamp(-128.0, 127.0) as i32 + i32::from(SILENCE)) as u8 {
        b';' => b':',
        b => b,
    }
}

struct Rx {
    dec: Decoder,
    pcm: VecDeque<i16>,
    heard: Option<Instant>,
    failed: Option<String>,
}

struct Shared {
    rx: Mutex<Rx>,
    ready: Condvar,
    closed: AtomicBool,
}

impl Shared {
    fn new() -> Self {
        Self {
            rx: Mutex::new(Rx { dec: Decoder::new(), pcm: VecDeque::new(), heard: None, failed: None }),
            ready: Condvar::new(),
            closed: AtomicBool::new(false),
        }
    }

    fn receive(&self, bytes: &[u8]) {
        let mut rx = lock(&self.rx);
        let Rx { dec, pcm, heard, .. } = &mut *rx;
        if dec.receive(bytes, pcm) {
            *heard = Some(Instant::now());
        }
        if pcm.len() > KEEP {
            let n = pcm.len() - KEEP_AFTER_DROP;
            pcm.drain(..n);
        }
        self.ready.notify_all();
    }

    fn new_stream(&self) -> u32 {
        let d = Decoder::new();
        std::mem::replace(&mut lock(&self.rx).dec, d).errors
    }

    fn fail(&self, why: String) {
        lock(&self.rx).failed.get_or_insert(why);
        self.ready.notify_all();
    }

    fn quiet_for(&self, d: Duration) -> bool {
        lock(&self.rx).heard.is_none_or(|t| t.elapsed() >= d)
    }
}

#[derive(Clone, Copy)]
struct Air {
    want: bool,
    on: bool,
    closed: bool,
}

struct Link {
    port: File,
    path: String,
    air: Mutex<Air>,
    changed: Condvar,
}

impl Link {
    fn send(&self, bytes: &[u8]) -> io::Result<()> {
        (&self.port).write_all(bytes).map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", self.path)))
    }

    fn key(&self) -> io::Result<bool> {
        let mut a = lock(&self.air);
        if a.closed {
            return Ok(false);
        }
        a.on = true;
        self.send(b";TX0;").map(|()| true)
    }

    fn unkey(&self, a: &mut Air, cmd: &[u8]) -> io::Result<()> {
        a.on = false;
        self.changed.notify_all();
        self.send(cmd)
    }
}

struct TrusdxPtt(Arc<Link>);

impl Ptt for TrusdxPtt {
    fn set(&mut self, on: bool) -> io::Result<()> {
        let l = &self.0;
        let mut a = lock(&l.air);
        a.want = on;
        if on {
            return Ok(());
        }
        let mut a = l.changed.wait_timeout_while(a, UNKEY_WAIT, |a| a.on).unwrap_or_else(PoisonError::into_inner).0;
        if a.on { l.unkey(&mut a, b";RX;") } else { Ok(()) }
    }

    fn release(&mut self) -> io::Result<()> {
        let l = &self.0;
        let mut a = lock(&l.air);
        (a.want, a.closed) = (false, true);
        l.unkey(&mut a, b";RX;")
    }

    fn describe(&self) -> String {
        format!("(tr)uSDX CAT on {}", self.0.path)
    }
}

fn configure(f: &File, path: &str) -> io::Result<()> {
    let fd = f.as_raw_fd();
    // SAFETY: t is a plain termios, and fd is open for the life of f.
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        t.c_cflag = libc::CS8 | libc::CREAD | libc::CLOCAL | libc::HUPCL;
        t.c_cc[libc::VMIN] = 1;
        t.c_cc[libc::VTIME] = 0;
        if libc::cfsetspeed(&mut t, libc::B115200) != 0 || libc::tcsetattr(fd, libc::TCSANOW, &t) != 0 {
            let e = io::Error::last_os_error();
            return Err(io::Error::other(format!("{path}: setting 115200 8N1: {e}")));
        }
        let fl = libc::fcntl(fd, libc::F_GETFL);
        if fl < 0 || libc::fcntl(fd, libc::F_SETFL, fl & !libc::O_NONBLOCK) != 0 {
            let e = io::Error::last_os_error();
            return Err(io::Error::new(e.kind(), format!("{path}: {e}")));
        }
    }
    let _ = modem(f, libc::TIOCMBIS, libc::TIOCM_DTR);
    let _ = modem(f, libc::TIOCMBIC, libc::TIOCM_RTS);
    Ok(())
}

fn outq(f: &File) -> usize {
    let mut n: libc::c_int = 0;
    // SAFETY: f is open and n is the int TIOCOUTQ writes.
    match unsafe { libc::ioctl(f.as_raw_fd(), libc::TIOCOUTQ, &mut n) } {
        0 => n.max(0) as usize,
        _ => 0,
    }
}

fn reader(mut f: File, s: Arc<Shared>, path: String) {
    let mut buf = [0u8; 1024];
    while !s.closed.load(Ordering::SeqCst) {
        let mut p = libc::pollfd { fd: f.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: p is one valid pollfd.
        if unsafe { libc::poll(&mut p, 1, 100) } <= 0 {
            continue;
        }
        match f.read(&mut buf) {
            Ok(0) => return s.fail(format!("{path}: serial port closed")),
            Ok(n) => s.receive(&buf[..n]),
            Err(e) if matches!(e.kind(), io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock) => {}
            Err(e) => return s.fail(format!("{path}: {e}")),
        }
    }
}

fn check_path(path: &str) -> io::Result<()> {
    if path.starts_with('/') {
        Ok(())
    } else {
        Err(io::Error::other(format!("--audio trusdx: {path:?} is not a serial device path")))
    }
}

pub fn check(playback: &str) -> io::Result<()> {
    check_path(playback)
}

#[cfg(target_os = "macos")]
const PORTS: &[&str] = &["cu.usbserial", "cu.usbmodem", "cu.wchusbserial"];
#[cfg(target_os = "freebsd")]
const PORTS: &[&str] = &["cuaU"];
#[cfg(not(any(target_os = "macos", target_os = "freebsd")))]
const PORTS: &[&str] = &["ttyUSB", "ttyACM"];

pub fn list() -> io::Result<(DeviceList, DeviceList)> {
    let mut ports: DeviceList = std::fs::read_dir("/dev")?
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| PORTS.iter().any(|p| n.starts_with(p)) && !n.ends_with(".init") && !n.ends_with(".lock"))
        .map(|n| (format!("/dev/{n}"), "serial port".to_string()))
        .collect();
    ports.sort();
    Ok((ports.clone(), ports))
}

#[derive(Clone, Copy)]
enum Tx {
    Idle { recheck: Option<Instant> },
    Stopping { since: Instant, again: bool },
    Keyed,
    Draining { empty: Option<Instant> },
}

pub struct TrusdxIo {
    shared: Arc<Shared>,
    link: Arc<Link>,
    playback: bool,
    radio: bool,
    speaker: bool,
    reader: Option<JoinHandle<()>>,
    tx: Tx,
    enc: Encoder,
    pending: Vec<i16>,
    bytes: Vec<u8>,
    last: Option<Instant>,
    asked: Instant,
}

impl TrusdxIo {
    pub fn open(capture: &str, playback: &str, radio: bool, speaker: bool) -> io::Result<Self> {
        if capture != playback {
            return Err(io::Error::other("--audio trusdx: --capture and --playback must be the same serial port"));
        }
        Self::connect(capture, true, radio, speaker)
    }

    pub fn open_capture(capture: &str) -> io::Result<Self> {
        Self::connect(capture, false, false, false)
    }

    fn connect(path: &str, playback: bool, radio: bool, speaker: bool) -> io::Result<Self> {
        check_path(path)?;
        let f = open_tty(path)?;
        configure(&f, path)?;
        let link = Arc::new(Link {
            port: f.try_clone()?,
            path: path.into(),
            air: Mutex::new(Air { want: false, on: false, closed: false }),
            changed: Condvar::new(),
        });
        let shared = Arc::new(Shared::new());
        let (s, p) = (shared.clone(), path.to_string());
        let mut io = Self {
            shared,
            link,
            playback,
            radio,
            speaker,
            reader: None,
            tx: Tx::Idle { recheck: None },
            enc: Encoder::new(),
            pending: Vec::new(),
            bytes: Vec::new(),
            last: None,
            asked: Instant::now(),
        };
        io.reader = Some(thread::Builder::new().name("trusdx".into()).spawn(move || reader(f, s, p))?);
        io.listen()?;
        Ok(io)
    }

    fn start(&self) -> &'static [u8] {
        match (self.radio, self.speaker) {
            (true, true) => b";RX;UA1;",
            (true, false) => b";RX;UA2;",
            (false, true) => b"UA1;",
            (false, false) => b"UA2;",
        }
    }

    fn listen(&mut self) -> io::Result<u32> {
        let errors = self.shared.new_stream();
        self.asked = Instant::now();
        self.link.unkey(&mut lock(&self.link.air), self.start())?;
        Ok(errors)
    }

    fn key(&mut self) -> io::Result<()> {
        self.shared.new_stream();
        self.enc = Encoder::new();
        if !self.link.key()? {
            return self.abandon();
        }
        self.tx = Tx::Keyed;
        Ok(())
    }

    fn abandon(&mut self) -> io::Result<()> {
        self.pending.clear();
        self.tx = Tx::Idle { recheck: None };
        self.listen().map(drop)
    }

    fn transmit(&mut self, x: &[i16]) -> io::Result<()> {
        self.bytes.clear();
        self.enc.encode(self.pending.drain(..).chain(x.iter().copied()), &mut self.bytes);
        self.link.send(&self.bytes)
    }

    fn unkeyed(&mut self) -> io::Result<()> {
        self.tx = Tx::Idle { recheck: Some(Instant::now() + RECHECK) };
        let e = self.listen()?;
        if e > 0 {
            info!("(tr)uSDX: radio answered E; {e} times during the transmission");
        }
        Ok(())
    }
}

impl Audio for TrusdxIo {
    fn read(&mut self, x: &mut [i16]) -> io::Result<()> {
        let block = Duration::from_secs_f64(x.len() as f64 / f64::from(RATE));
        let due = self.last.map_or_else(Instant::now, |t| t + block);
        let idle = matches!(self.tx, Tx::Idle { .. });
        let mut rx = lock(&self.shared.rx);
        loop {
            if let Some(why) = &rx.failed {
                return Err(io::Error::other(why.clone()));
            }
            if rx.pcm.len() >= x.len() {
                break;
            }
            let now = Instant::now();
            let streaming = idle && rx.heard.is_some_and(|t| now.duration_since(t) < STREAMING);
            let until = if streaming { due + JITTER } else { due };
            let Some(left) = until.checked_duration_since(now).filter(|d| !d.is_zero()) else { break };
            rx = self.shared.ready.wait_timeout(rx, left).unwrap_or_else(PoisonError::into_inner).0;
        }
        let n = rx.pcm.len().min(x.len());
        for (o, s) in x.iter_mut().zip(rx.pcm.drain(..n).chain(std::iter::repeat(0))) {
            *o = s;
        }
        let quiet = rx.heard.is_none_or(|t| t.elapsed() >= RESTART);
        drop(rx);
        let now = Instant::now();
        self.last = Some(if n == x.len() || now > due + STREAMING { now } else { due });
        if idle && quiet && self.asked.elapsed() >= RESTART {
            self.listen()?;
        }
        Ok(())
    }

    fn write(&mut self, x: &[i16]) -> io::Result<()> {
        if !self.playback {
            return Err(capture_only());
        }
        if !self.radio {
            return Ok(());
        }
        let now = Instant::now();
        let Air { want, on, closed } = *lock(&self.link.air);
        let (want, sound) = (want && !closed, !closed && x.iter().any(|&s| s != 0));
        match self.tx {
            Tx::Keyed | Tx::Draining { .. } if !on => self.unkeyed(),
            Tx::Idle { recheck } => {
                if want || sound {
                    self.pending.extend_from_slice(x);
                    self.tx = Tx::Stopping { since: now, again: false };
                    return self.link.send(b";UA0;");
                }
                if recheck.is_some_and(|t| now >= t) {
                    self.tx = Tx::Idle { recheck: None };
                    return self.link.send(b";RX;");
                }
                Ok(())
            }
            Tx::Stopping { since, again } => {
                self.pending.extend_from_slice(x);
                if closed || !want && self.pending.iter().all(|&s| s == 0) {
                    return self.abandon();
                }
                let waited = now.duration_since(since);
                if waited >= KEY_WAIT || self.shared.quiet_for(STREAM_IDLE) {
                    return self.key();
                }
                if !again && waited >= KEY_WAIT / 2 {
                    self.tx = Tx::Stopping { since, again: true };
                    return self.link.send(b";UA0;");
                }
                Ok(())
            }
            Tx::Keyed => {
                if !want && !sound {
                    self.tx = Tx::Draining { empty: None };
                }
                self.transmit(x)
            }
            Tx::Draining { empty } => {
                if want || sound {
                    self.tx = Tx::Keyed;
                    return self.transmit(x);
                }
                match empty {
                    Some(t) if now.duration_since(t) >= SETTLE => self.unkeyed(),
                    Some(_) => Ok(()),
                    None => {
                        if outq(&self.link.port) == 0 {
                            self.tx = Tx::Draining { empty: Some(now) };
                        }
                        Ok(())
                    }
                }
            }
        }
    }

    fn queued(&mut self) -> io::Result<Option<Queue>> {
        if !self.playback {
            return Err(capture_only());
        }
        Ok(None)
    }

    fn backlog(&mut self) -> io::Result<Option<usize>> {
        Ok(Some(lock(&self.shared.rx).pcm.len()))
    }

    fn path(&mut self) -> io::Result<Path> {
        let keying = if self.radio { super::samples(KEYING) } else { 0 };
        Ok(Path { keying, ..Path::default() })
    }

    fn ptt(&self) -> Option<Box<dyn Ptt>> {
        self.radio.then(|| Box::new(TrusdxPtt(self.link.clone())) as Box<dyn Ptt>)
    }
}

impl Drop for TrusdxIo {
    fn drop(&mut self) {
        let (cmd, what): (&[u8], _) = if self.radio { (b";RX;UA0;", "unkey") } else { (b"UA0;", "stop audio") };
        if let Err(e) = self.link.unkey(&mut lock(&self.link.air), cmd) {
            say!("(tr)uSDX: could not {what}: {e}");
        }
        self.shared.closed.store(true, Ordering::SeqCst);
        if let Some(r) = self.reader.take() {
            let _ = r.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::BLOCK;
    use std::ffi::CStr;
    use std::fs::OpenOptions;
    use std::os::fd::FromRawFd;
    use std::os::unix::fs::OpenOptionsExt;

    #[test]
    fn cat_replies_come_apart_from_audio() {
        let mut d = Decoder::new();
        let mut pcm = VecDeque::new();
        assert!(d.receive(b"UA2;US\x80\x81", &mut pcm));
        assert!(d.audio);
        assert!(d.receive(b";ID020;E;FA00007074000;US\x82", &mut pcm));
        assert!(!d.receive(b";UA1;E;", &mut pcm));
        assert_eq!((d.audio, d.errors), (false, 2));
    }

    #[test]
    fn a_stream_joined_midway_is_still_heard() {
        let mut d = Decoder::new();
        let mut pcm = VecDeque::new();
        assert!(!d.receive(&[0xC0; CAT_MAX], &mut pcm));
        assert!(d.receive(&[0xC0; 2], &mut pcm));
        assert!(d.audio);
        assert!(!d.receive(b";E;", &mut pcm));
        assert_eq!(d.errors, 1);
    }

    #[test]
    fn each_stream_starts_a_fresh_decoder() {
        let s = Shared::new();
        s.receive(b"US\x80\x80");
        s.receive(b"E;");
        assert_eq!(s.new_stream(), 0, "a stream stopped without a semicolon swallows the reply");
        s.receive(b"E;");
        assert_eq!(s.new_stream(), 1);
    }

    #[test]
    fn received_audio_is_upsampled_to_the_modem_rate() {
        let mut d = Decoder::new();
        let mut pcm = VecDeque::new();
        let mut stream = b"US".to_vec();
        stream.extend(
            (0..RX_RATE as usize)
                .map(|i| byte(0.5 * (2.0 * std::f64::consts::PI * 1500.0 * i as f64 / f64::from(RX_RATE)).sin())),
        );
        d.receive(&stream, &mut pcm);
        let n = pcm.len() as f64;
        assert!((n - f64::from(RATE)).abs() < 200.0, "{n} samples from a second");
        let rms = (pcm.iter().skip(1000).map(|&s| sample(s).powi(2)).sum::<f64>() / (n - 1000.0)).sqrt();
        assert!((rms - 0.5 / 2f64.sqrt()).abs() < 0.02, "rms {rms}");
    }

    #[test]
    fn transmit_samples_never_carry_a_semicolon() {
        let mut e = Encoder::new();
        let mut out = Vec::new();
        e.encode((0..4800).map(|i| (i as i16).wrapping_mul(97)), &mut out);
        assert!((1100..=1152).contains(&out.len()), "{} bytes for 100 ms", out.len());
        assert!(!out.contains(&b';'));
        assert_eq!(byte(f64::from(i32::from(b';') - 128) / 128.0), b':');
    }

    struct Radio(File);

    impl Radio {
        fn open() -> (Self, String) {
            // SAFETY: openpty returns two new fds and writes a NUL-terminated name into buf.
            unsafe {
                let (mut fd, mut tty) = (-1, -1);
                let mut buf = [0 as libc::c_char; 64];
                let (t, w) = (std::ptr::null_mut(), std::ptr::null_mut());
                assert_eq!(libc::openpty(&mut fd, &mut tty, buf.as_mut_ptr(), t, w), 0);
                libc::close(tty);
                let name = CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned();
                let mut t: libc::termios = std::mem::zeroed();
                libc::tcgetattr(fd, &mut t);
                libc::cfmakeraw(&mut t);
                libc::tcsetattr(fd, libc::TCSANOW, &t);
                libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK);
                (Self(File::from_raw_fd(fd)), name)
            }
        }

        fn take(&mut self) -> Vec<u8> {
            thread::sleep(Duration::from_millis(5));
            let (mut out, mut buf) = (Vec::new(), [0u8; 4096]);
            while let Ok(n) = self.0.read(&mut buf) {
                if n == 0 {
                    break;
                }
                out.extend_from_slice(&buf[..n]);
            }
            out
        }
    }

    fn tone() -> Vec<i16> {
        (0..BLOCK).map(|i| (8000.0 * (i as f64 * 0.2).sin()) as i16).collect()
    }

    fn termios(f: &File) -> libc::termios {
        // SAFETY: f is open and t is a termios that tcgetattr fills in.
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            assert_eq!(libc::tcgetattr(f.as_raw_fd(), &mut t), 0);
            t
        }
    }

    #[test]
    fn settings_left_on_the_port_are_replaced() {
        let (_radio, path) = Radio::open();
        let port = OpenOptions::new().read(true).write(true).custom_flags(libc::O_NOCTTY).open(&path).unwrap();
        let mut t = termios(&port);
        t.c_cflag |= libc::CRTSCTS | libc::CSTOPB | libc::PARENB;
        t.c_iflag |= libc::IXON | libc::IXOFF | libc::ICRNL;
        t.c_lflag |= libc::ICANON | libc::ECHO | libc::ISIG;
        t.c_oflag |= libc::OPOST;
        // SAFETY: port is open and t came from tcgetattr.
        assert_eq!(unsafe { libc::tcsetattr(port.as_raw_fd(), libc::TCSANOW, &t) }, 0);
        let _io = TrusdxIo::open_capture(&path).unwrap();
        let t = termios(&port);
        assert_eq!(t.c_cflag & (libc::CRTSCTS | libc::CSTOPB | libc::PARENB), 0);
        assert_eq!(t.c_cflag & libc::CSIZE, libc::CS8);
        assert_eq!((t.c_iflag, t.c_oflag, t.c_lflag), (0, 0, 0));
        // SAFETY: t is a valid termios.
        assert_eq!(unsafe { libc::cfgetospeed(&t) }, libc::B115200);
    }

    fn unkey(mut ptt: Box<dyn Ptt>) -> JoinHandle<(Box<dyn Ptt>, Duration)> {
        let h = thread::spawn(move || {
            let t = Instant::now();
            ptt.set(false).unwrap();
            (ptt, t.elapsed())
        });
        thread::sleep(Duration::from_millis(5));
        h
    }

    #[test]
    fn keying_stops_the_stream_then_sends_samples_then_resumes_it() {
        let (mut radio, path) = Radio::open();
        let mut io = TrusdxIo::open(&path, &path, true, false).unwrap();
        let mut ptt = io.ptt().unwrap();
        assert_eq!(radio.take(), b";RX;UA2;");

        let mut stream = b"UA2;US".to_vec();
        stream.extend(std::iter::repeat_n(0xC0u8, RX_RATE as usize / 10));
        radio.0.write_all(&stream).unwrap();
        thread::sleep(Duration::from_millis(20));
        let mut x = [0i16; BLOCK];
        io.read(&mut x).unwrap();
        assert!(x[200..].iter().all(|&s| s > 12000), "{:?}", &x[200..210]);

        let (quiet, loud) = ([0i16; BLOCK], tone());
        io.write(&quiet).unwrap();
        assert!(radio.take().is_empty(), "nothing is sent unkeyed");

        ptt.set(true).unwrap();
        io.write(&loud).unwrap();
        assert_eq!(radio.take(), b";UA0;");
        thread::sleep(STREAM_IDLE);
        io.write(&loud).unwrap();
        assert_eq!(radio.take(), b";TX0;");
        io.write(&loud).unwrap();
        let sent = radio.take();
        assert!(sent.len() > 300 && !sent.contains(&b';'), "{} bytes", sent.len());

        let off = unkey(ptt);
        io.write(&loud).unwrap();
        assert!(!radio.take().contains(&b';'), "audio after PTT drops is still sent");
        io.write(&quiet).unwrap();
        assert!(!radio.take().contains(&b';'));
        assert!(!off.is_finished(), "PTT off waits for the radio to unkey");
        let mut unkeyed = Vec::new();
        for _ in 0..4 {
            io.write(&quiet).unwrap();
            unkeyed.extend(radio.take());
            thread::sleep(SETTLE);
        }
        assert_eq!(unkeyed, b";RX;UA2;", "unkey and stream again at once");
        let (mut ptt, took) = off.join().unwrap();
        assert!(took < UNKEY_WAIT, "{took:?}");
        thread::sleep(RECHECK);
        io.write(&quiet).unwrap();
        assert_eq!(radio.take(), b";RX;", "the unkey is repeated once");

        ptt.set(true).unwrap();
        io.write(&quiet).unwrap();
        assert_eq!(radio.take(), b";UA0;");
        ptt.set(false).unwrap();
        io.write(&quiet).unwrap();
        assert_eq!(radio.take(), b";RX;UA2;", "a PTT gone before TX0 never keys");

        ptt.set(true).unwrap();
        io.write(&loud).unwrap();
        thread::sleep(STREAM_IDLE);
        io.write(&loud).unwrap();
        assert!(radio.take().ends_with(b";TX0;"));
        let (mut ptt, took) = unkey(ptt).join().unwrap();
        assert!(took >= UNKEY_WAIT, "{took:?}");
        assert_eq!(radio.take(), b";RX;", "a stalled loop is unkeyed from the PTT thread");
        io.write(&loud).unwrap();
        assert_eq!(radio.take(), b";RX;UA2;", "the loop sees the radio was unkeyed");

        ptt.set(true).unwrap();
        io.write(&loud).unwrap();
        thread::sleep(STREAM_IDLE);
        io.write(&loud).unwrap();
        assert!(radio.take().ends_with(b";TX0;"));
        let t = Instant::now();
        ptt.release().unwrap();
        assert!(t.elapsed() < Duration::from_millis(50));
        assert_eq!(radio.take(), b";RX;", "release unkeys at once");

        drop(io);
        assert!(radio.take().ends_with(b";RX;UA0;"));
    }

    fn keyed(radio: &mut Radio, io: &mut TrusdxIo, ptt: &mut Box<dyn Ptt>) {
        let loud = tone();
        ptt.set(true).unwrap();
        io.write(&loud).unwrap();
        thread::sleep(STREAM_IDLE);
        io.write(&loud).unwrap();
        assert!(radio.take().ends_with(b";TX0;"));
    }

    #[test]
    fn nothing_keys_the_radio_after_release() {
        for stopping in [true, false] {
            let (mut radio, path) = Radio::open();
            let mut io = TrusdxIo::open(&path, &path, true, false).unwrap();
            let mut ptt = io.ptt().unwrap();
            radio.take();
            let loud = tone();
            if stopping {
                ptt.set(true).unwrap();
                io.write(&loud).unwrap();
                assert_eq!(radio.take(), b";UA0;");
            } else {
                keyed(&mut radio, &mut io, &mut ptt);
            }
            ptt.release().unwrap();
            assert_eq!(radio.take(), b";RX;", "{stopping}");
            thread::sleep(STREAM_IDLE);
            ptt.set(true).unwrap();
            io.write(&loud).unwrap();
            assert_eq!(radio.take(), b";RX;UA2;", "{stopping}");
            for _ in 0..20 {
                io.write(&loud).unwrap();
                thread::sleep(SETTLE);
            }
            let rest = radio.take();
            assert!(!rest.windows(4).any(|w| w == b"TX0;" || w == b"UA0;"), "{stopping}: {rest:?}");
            drop(io);
            assert_eq!(radio.take(), b";RX;UA0;");
        }
    }

    #[test]
    fn ptt_off_waits_out_audio_that_keys_again_while_draining() {
        let (mut radio, path) = Radio::open();
        let mut io = TrusdxIo::open(&path, &path, true, false).unwrap();
        let mut ptt = io.ptt().unwrap();
        radio.take();
        keyed(&mut radio, &mut io, &mut ptt);
        let off = unkey(ptt);
        io.write(&[0; BLOCK]).unwrap();
        io.write(&tone()).unwrap();
        assert!(!radio.take().contains(&b';'));
        let (_ptt, took) = off.join().unwrap();
        assert!(took >= UNKEY_WAIT, "{took:?}");
        assert!(!lock(&io.link.air).on);
        assert_eq!(radio.take(), b";RX;");
    }

    #[test]
    fn a_lost_port_fails_the_read() {
        let (radio, path) = Radio::open();
        let mut io = TrusdxIo::open_capture(&path).unwrap();
        drop(radio);
        let mut x = [0i16; BLOCK];
        let t = Instant::now();
        while io.read(&mut x).is_ok() {
            assert!(t.elapsed() < Duration::from_secs(2), "still reading");
        }
    }

    #[test]
    fn without_audio_the_read_keeps_the_wall_clock() {
        let (_radio, path) = Radio::open();
        let mut io = TrusdxIo::open_capture(&path).unwrap();
        let mut x = [1i16; BLOCK];
        let t = Instant::now();
        for _ in 0..20 {
            io.read(&mut x).unwrap();
        }
        let took = t.elapsed();
        assert!(x.iter().all(|&s| s == 0));
        assert!(took >= Duration::from_millis(190) && took < Duration::from_millis(300), "{took:?}");
        assert!(io.write(&x).is_err());
        assert!(io.ptt().is_none());
    }
}
