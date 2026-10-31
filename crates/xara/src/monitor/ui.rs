use super::record::Record;
use super::{Line, Monitor};
use crate::lock;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use xara_arq::kiss;
use xara_phy::hf::level::Bandwidth;
use xara_phy::hf::rx::FS;
use xara_ui::bus::Publisher;
use xara_ui::json::Obj;
use xara_ui::messages::meter;
use xara_ui::telemetry::Meter;

const HEARD_KEEP: usize = 64;

#[derive(Clone, Debug, PartialEq)]
struct Station {
    name: String,
    kinds: Vec<&'static str>,
    first: f64,
    last: f64,
    count: usize,
    bw: Option<String>,
}

#[derive(Default)]
pub struct Heard(Vec<Station>);

impl Heard {
    pub fn note(&mut self, t: f64, r: &Record) {
        let (name, kind, bw) = match r {
            Record::Ctrl { key, .. } => (key.name.clone(), key.kind(), None),
            Record::Header { call, .. } => (call.clone(), "caller", None),
            Record::Cq { call: Some(c), bw, .. } => (c.clone(), "cq", bw.clone()),
            _ => return,
        };
        match self.0.iter_mut().find(|s| s.name == name) {
            Some(s) => {
                s.last = s.last.max(t);
                s.count += 1;
                if !s.kinds.contains(&kind) {
                    s.kinds.push(kind);
                }
                if bw.is_some() {
                    s.bw = bw;
                }
            }
            None => self.0.push(Station { name, kinds: vec![kind], first: t, last: t, count: 1, bw }),
        }
        if self.0.len() > HEARD_KEEP {
            self.0.sort_by(|a, b| b.last.total_cmp(&a.last));
            self.0.truncate(HEARD_KEEP);
        }
    }

    fn json(&self) -> String {
        let mut v: Vec<&Station> = self.0.iter().collect();
        v.sort_by(|a, b| b.last.total_cmp(&a.last));
        let items: Vec<String> = v
            .iter()
            .map(|s| {
                let kinds: Vec<String> = s.kinds.iter().map(|k| k.to_string()).collect();
                Obj::new()
                    .str("name", &s.name)
                    .strs("kinds", &kinds)
                    .num2("first", s.first)
                    .num2("last", s.last)
                    .int("count", s.count as u64)
                    .opt_str("bw", s.bw.as_deref())
                    .end()
            })
            .collect();
        format!("[{}]", items.join(","))
    }
}

pub fn decoder_json(m: &Monitor, heard: &Heard) -> String {
    let link = m.link().map_or("null".into(), |l| {
        Obj::new()
            .opt_str("caller", l.caller.as_deref())
            .opt_str("answerer", l.answerer.as_deref())
            .num2("start", l.start)
            .nums2("bytes", &l.bytes.map(|b| b as f64))
            .end()
    });
    let keys = m
        .keys()
        .into_iter()
        .map(|k| k.key.put(Obj::new()).str("origin", k.origin).num2("last", k.last).opt2("expires", k.expires));
    Obj::new()
        .num2("decoded", m.now())
        .raw("link", &link)
        .arr("keys", keys)
        .raw("heard", &heard.json())
        .obj("stats", m.stats.put(Obj::new(), false))
        .end()
}

pub struct Shared {
    pub publisher: Publisher,
    source: String,
    bw: Bandwidth,
    epoch: Option<f64>,
    meter: Mutex<Meter>,
    sent: AtomicU64,
    decoder: Mutex<Decoded>,
    ended: AtomicBool,
}

#[derive(Clone, Default)]
struct Decoded {
    at: f64,
    json: String,
}

impl Shared {
    pub fn new(publisher: Publisher, source: &str, bw: Bandwidth, epoch: Option<f64>) -> Self {
        Self {
            publisher,
            source: source.into(),
            bw,
            epoch,
            meter: Mutex::default(),
            sent: AtomicU64::new(0),
            decoder: Mutex::default(),
            ended: AtomicBool::new(false),
        }
    }

    pub fn input(&self, x: &[f64]) {
        lock(&self.meter).feed(x);
        self.publisher.audio(x);
        self.sent.fetch_add(x.len() as u64, Ordering::Relaxed);
    }

    pub fn line(&self, l: &Line, heard: &mut Heard) {
        heard.note(l.t, &l.rec);
        self.publisher.event(l.rec.json(l.t, self.epoch.map(|e| e + l.t), &l.text));
    }

    pub fn decoded(&self, m: &Monitor, heard: &Heard) {
        *lock(&self.decoder) = Decoded { at: m.now(), json: decoder_json(m, heard) };
    }

    pub fn end(&self) {
        self.ended.store(true, Ordering::Relaxed);
    }

    pub fn status(&self) -> String {
        let ts = self.sent.load(Ordering::Relaxed) as f64 / FS;
        let rx = std::mem::take(&mut *lock(&self.meter));
        let Decoded { at: decoded, json: dec } = lock(&self.decoder).clone();
        let (lo, hi) = self.bw.passband();
        let (ulo, uhi) = kiss::BW.passband();
        Obj::typed("status")
            .num2("ts", ts)
            .opt2("epoch", self.epoch)
            .str("source", &self.source)
            .int("bw", u64::from(self.bw.hz()))
            .nums2("passband", &[lo, hi])
            .nums2("unproto", &[ulo, uhi])
            .obj("rx", meter(&rx))
            .num2("lag", (ts - decoded).max(0.0))
            .bool("ended", self.ended.load(Ordering::Relaxed))
            .int("dropped", self.publisher.dropped() as u64)
            .raw("decoder", if dec.is_empty() { "null" } else { &dec })
            .end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::MonitorOpts;
    use crate::monitor::record::KeyRef;
    use xara_arq::cq::{Bw, Cq};
    use xara_phy::hf::ctrl::{Fmt, Role};
    use xara_phy::hf::rx::blind::Key;

    fn ctrl(name: &str, key: Key) -> Record {
        Record::Ctrl {
            sender: Role::Caller,
            code: 26,
            fmt: Fmt::F0,
            hits: 31,
            total: 31,
            snr: None,
            df: 0.0,
            blind: false,
            key: KeyRef { key, name: name.into() },
        }
    }

    #[test]
    fn heard_merges_names_and_keeps_the_latest() {
        let mut h = Heard::default();
        h.note(1.0, &ctrl("#D8A3D7", Key::Answerer(0xD8A3D7)));
        h.note(2.0, &Record::cq(&Cq { call: "N2CALL-9".into(), bw: Some(Bw::Known(Bandwidth::Hz500)) }));
        h.note(3.0, &ctrl("#D8A3D7", Key::Answerer(0xD8A3D7)));
        h.note(
            4.0,
            &Record::Header { call: "N0CALL".into(), answerer: "#D8A3D7".into(), flags: 1, start: 4, marker_ok: true },
        );
        h.note(5.0, &ctrl("N0CALL", Key::Caller(1)));
        h.note(6.0, &Record::cq(&Cq { call: String::new(), bw: None }));
        assert_eq!(
            h.json(),
            r##"[{"name":"N0CALL","kinds":["caller"],"first":4,"last":5,"count":2,"bw":null},{"name":"#D8A3D7","kinds":["answerer"],"first":1,"last":3,"count":2,"bw":null},{"name":"N2CALL-9","kinds":["cq"],"first":2,"last":2,"count":1,"bw":"500"}]"##
        );
        for i in 0..HEARD_KEEP + 10 {
            h.note(10.0 + i as f64, &Record::cq(&Cq { call: format!("C{i}"), bw: Some(Bw::Known(Bandwidth::Hz500)) }));
        }
        assert_eq!(h.0.len(), HEARD_KEEP);
        assert!(h.0.iter().all(|s| s.name.starts_with('C') && s.name != "C0"));
    }

    #[test]
    fn status_reports_input_lag_and_resets_the_meter() {
        let (p, _sub) = xara_ui::bus::channel(4);
        let s = Shared::new(p, "stdin", Bandwidth::Hz500, Some(1000.0));
        s.input(&[0.5; 48_000]);
        let m = Monitor::new(Bandwidth::Hz500, &MonitorOpts { blind: false, ..Default::default() });
        s.decoded(&m, &Heard::default());
        let v: xara_testkit::json::Value = xara_testkit::json::from_str(&s.status()).unwrap();
        assert_eq!((v["ts"].as_f64(), v["lag"].as_f64(), v["epoch"].as_f64()), (Some(1.0), Some(1.0), Some(1000.0)));
        assert_eq!((v["passband"][0].as_f64(), v["passband"][1].as_f64()), (Some(1250.0), Some(1750.0)));
        assert_eq!((v["unproto"][0].as_f64(), v["unproto"][1].as_f64()), (Some(1250.0), Some(1750.0)));
        assert_eq!((v["rx"]["rms"].as_f64(), v["decoder"]["link"].is_null()), (Some(-6.02), true));
        let again: xara_testkit::json::Value = xara_testkit::json::from_str(&s.status()).unwrap();
        assert_eq!(again["rx"]["rms"].as_f64(), Some(-120.0));
        s.end();
        assert!(s.status().contains(r#""ended":true"#));
    }
}
