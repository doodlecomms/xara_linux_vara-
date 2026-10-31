use std::f64::consts::PI;
use std::time::{Duration, Instant};
use xara::audio::ratematch::TxBuffer;
use xara::audio::txpath::TxPath;
use xara::audio::{self, BLOCK, Backend, RATE};

const CHIRP: usize = 960;
const SPACING: usize = RATE as usize / BLOCK;
const CHIRPS: usize = 8;
const SEARCH: usize = RATE as usize / 2;
const OFFSET: usize = 100;

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.is_empty())
}

fn chirp() -> Vec<f64> {
    let (f0, f1, n) = (600.0, 2600.0, CHIRP as f64);
    (0..CHIRP)
        .map(|i| {
            let t = i as f64 / f64::from(RATE);
            let w = (PI * i as f64 / n).sin().powi(2);
            0.5 * w * (2.0 * PI * (f0 * t + (f1 - f0) * t * t * f64::from(RATE) / (2.0 * n))).sin()
        })
        .collect()
}

fn find(rx: &[f64], c: &[f64], from: usize) -> Option<(usize, f64)> {
    let energy = c.iter().map(|v| v * v).sum::<f64>();
    let end = (from + SEARCH).min(rx.len().checked_sub(c.len())?);
    (from..end)
        .map(|lag| {
            let dot = rx[lag..lag + c.len()].iter().zip(c).map(|(a, b)| a * b).sum::<f64>();
            let power = rx[lag..lag + c.len()].iter().map(|v| v * v).sum::<f64>();
            (lag, dot / (energy * power).sqrt().max(1e-12))
        })
        .max_by(|a, b| a.1.total_cmp(&b.1))
}

#[test]
#[ignore = "needs a loopback: set XARA_LOOP_AUDIO, XARA_LOOP_CAPTURE and XARA_LOOP_PLAYBACK"]
fn the_tx_budget_matches_a_loopback() {
    let (Some(b), Some(capture), Some(playback)) =
        (env("XARA_LOOP_AUDIO"), env("XARA_LOOP_CAPTURE"), env("XARA_LOOP_PLAYBACK"))
    else {
        panic!("set XARA_LOOP_AUDIO, XARA_LOOP_CAPTURE and XARA_LOOP_PLAYBACK");
    };
    let b: Backend = b.parse().unwrap();
    let quantum = env("XARA_LOOP_QUANTUM").map_or(if b == Backend::Pulse { 256 } else { 0 }, |v| v.parse().unwrap());
    let radio = env("XARA_LOOP_RADIO").is_some();
    let buf = env("XARA_LOOP_TX_BUFFER_MS").map_or_else(TxBuffer::default, |v| TxBuffer::ms(v.parse().unwrap()));
    let mut io = audio::open(b, &capture, &playback, radio, buf, false).unwrap();
    let mut tx = TxPath::new(0, buf, io.path().unwrap());
    eprintln!("start: {}", tx.budget());
    tx.start(io.as_mut()).unwrap();
    let c = chirp();
    let (mut rx, mut block) = (Vec::new(), vec![0i16; BLOCK]);
    let mut out = vec![0.0; BLOCK];
    let (mut sent, mut first, mut carry) = (Vec::new(), None, Vec::new());
    let settle = Duration::from_secs(env("XARA_LOOP_SETTLE_S").map_or(4, |v| v.parse().unwrap()));
    let begun = Instant::now();
    let mut backlogs = Vec::new();
    for k in 0.. {
        let reading = Instant::now();
        io.read(&mut block).unwrap();
        let waited = reading.elapsed();
        let t0 = rx.len();
        rx.extend(block.iter().map(|&v| audio::sample(v)));
        let backlog = io.backlog().unwrap();
        if first.is_some() {
            backlogs.push(backlog.unwrap_or(0));
        }
        out.fill(0.0);
        let n = carry.len().min(BLOCK);
        out[..n].copy_from_slice(&carry[..n]);
        carry.drain(..n);
        if first.is_none() && begun.elapsed() >= settle {
            first = Some(k);
        }
        let due = first.is_some_and(|f| k >= f && (k - f) % SPACING == 0 && (k - f) / SPACING < CHIRPS);
        if due {
            let n = (BLOCK - OFFSET).min(CHIRP);
            out[OFFSET..OFFSET + n].copy_from_slice(&c[..n]);
            carry.extend_from_slice(&c[n..]);
        }
        if let Some(b) = tx.send(io.as_mut(), &mut out, false, waited, backlog).unwrap() {
            eprintln!("block {k}: {b}");
        }
        if due {
            sent.push((t0, tx.budget().total()));
        }
        if sent.len() == CHIRPS && rx.len() >= sent[CHIRPS - 1].0 + SEARCH + CHIRP {
            break;
        }
    }
    let tolerance = (2 * RATE / 1000) as i64 + BLOCK as i64;
    let mut off = Vec::new();
    for &(t0, budget) in &sent {
        let at = t0 + OFFSET;
        let (found, score) = find(&rx, &c, at).unwrap();
        let measured = (found - at) as i64;
        eprintln!(
            "chirp at {at}: heard {:.2} ms later, budget {:.2} ms, off {:+.2} ms, match {score:.3}",
            measured as f64 * 1e3 / f64::from(RATE),
            budget as f64 * 1e3 / f64::from(RATE),
            (measured - budget) as f64 * 1e3 / f64::from(RATE)
        );
        assert!(score > 0.8, "chirp at {at} not found: best match {score:.3}");
        off.push(measured - budget);
    }
    eprintln!("final: {}", tx.budget());
    backlogs.sort_unstable();
    eprintln!(
        "capture backlog: mean {:.2} ms, median {:.2} ms, max {:.2} ms",
        backlogs.iter().sum::<usize>() as f64 / backlogs.len() as f64 * 1e3 / f64::from(RATE),
        backlogs[backlogs.len() / 2] as f64 * 1e3 / f64::from(RATE),
        backlogs[backlogs.len() - 1] as f64 * 1e3 / f64::from(RATE)
    );
    let unreported = 3 * quantum / 2;
    assert!(
        off.iter().all(|&o| -tolerance <= o && o <= tolerance + unreported),
        "budget off by {off:?} frames, allowed -{tolerance}..{} with {unreported} unreported for a quantum of {quantum}",
        tolerance + unreported
    );
}
