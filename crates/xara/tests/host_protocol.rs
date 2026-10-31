mod common;
use common::{Client, Lab, ask, check_sequence, events, is_reply, vector};
use xara::station::Modem;
use xara::station::hf::Station;
use xara::tnc::Tnc;
use xara_arq::hf::session::Config;
use xara_arq::hf::session::State;
use xara_host::server::Host;
use xara_phy::hf::level::Bandwidth;
use xara_testkit::net::turned_away;

fn solo() -> (Lab, Client) {
    let mut lab = Lab::new(1);
    let mut c = lab.attach(0);
    lab.settle();
    assert_eq!(ask(&mut lab, &mut c, b"MYCALL N0CALL\r", 1), ["OK"]);
    (lab, c)
}

fn idle_cases() -> Vec<(Vec<u8>, Vec<String>)> {
    let mut cases = Vec::new();
    for v in vector("host/commands.json")["vectors"].as_array().unwrap() {
        if v["state"] == "idle" {
            let line = format!("{}\r", v["line"].as_str().unwrap()).into_bytes();
            cases.push((line, v["expect"].as_array().unwrap().iter().map(|e| e.as_str().unwrap().into()).collect()));
        }
    }
    for t in vector("host/command_replies.json")["table"].as_array().unwrap() {
        let r = t["replies"].as_array().unwrap();
        let sent = t["sent"].as_str().unwrap().as_bytes().to_vec();
        if t["state"] != "idle" || r.len() != 1 || sent.starts_with(b"CLEANTXBUFFER") || !sent.ends_with(b"\r") {
            continue;
        }
        let want = r[0].as_str().unwrap();
        cases.push((sent, want.split(" | ").map(String::from).collect()));
    }
    cases
}

#[test]
fn idle_replies_match_vectors() {
    let cases = idle_cases();
    assert!(cases.len() > 120, "{}", cases.len());
    for (sent, want) in cases {
        let (mut lab, mut c) = solo();
        let got = ask(&mut lab, &mut c, &sent, want.len());
        assert_eq!(got, want, "{:?}", String::from_utf8_lossy(&sent));
    }
}

#[test]
fn command_line_framing() {
    let (mut lab, mut c) = solo();
    assert!(ask(&mut lab, &mut c, b"VERSION\n", 1).is_empty());
    assert_eq!(ask(&mut lab, &mut c, b"\r", 1), ["VERSION VARA HF v4.9.0"]);
    assert_eq!(ask(&mut lab, &mut c, b"\r", 1), ["WRONG"]);
    assert_eq!(ask(&mut lab, &mut c, b"VERSION\r\n", 1), ["VERSION VARA HF v4.9.0"]);
    assert_eq!(ask(&mut lab, &mut c, b"LISTEN ON\nVERSION\r", 1), ["WRONG"]);
    assert_eq!(ask(&mut lab, &mut c, b"CLEANTXBUFFER\r", 1), ["CLEANTXBUFFER BUFFEREMPTY"]);
    c.write(b"hello");
    assert!(lab.wait_for(|l| l.tnc[0].station().buffered() == 5));
    assert_eq!(ask(&mut lab, &mut c, b"ABORT\r", 1), ["OK"]);
    assert_eq!(ask(&mut lab, &mut c, b"DISCONNECT\r", 1), ["OK"]);
    assert_eq!(ask(&mut lab, &mut c, b"CLEANTXBUFFER\r", 1), ["CLEANTXBUFFER FAILED "]);
}

#[test]
fn retries_chain() {
    let (mut lab, mut c) = solo();
    let r = |lab: &Lab| lab.tnc[0].station().config().host.retries;
    for (cmd, want) in [("RETRIES 5", 5), ("RETRIES 7", 7), ("RETRIES 0", 5), ("RETRIES 300", 5), ("RETRIES -1", 5)] {
        assert_eq!(ask(&mut lab, &mut c, format!("{cmd}\r").as_bytes(), 1), ["OK"]);
        assert_eq!(r(&lab), want, "{cmd}");
    }
    let (mut lab, mut c) = solo();
    ask(&mut lab, &mut c, b"RETRIES 2\r", 1);
    ask(&mut lab, &mut c, b"RETRIES abc\r", 1);
    assert_eq!(r(&lab), 15);
}

#[test]
fn session_commands_touch_only_the_window() {
    let (mut lab, mut c) = solo();
    ask(&mut lab, &mut c, b"P2P SESSION\r", 1);
    let cfg = lab.tnc[0].station().config().clone();
    assert!(cfg.p2p && !cfg.host.chat);
    ask(&mut lab, &mut c, b"CHAT ON\r", 1);
    assert!(lab.tnc[0].station().config().host.chat);
    ask(&mut lab, &mut c, b"WINLINK SESSION\r", 1);
    let cfg = lab.tnc[0].station().config().clone();
    assert!(!cfg.p2p && cfg.host.chat);
}

fn link(lab: &mut Lab, a: &mut Client, b: &mut Client) {
    lab.settle();
    for c in ["MYCALL N0CALL", "BW2300", "CHAT OFF", "COMPRESSION OFF"] {
        a.send(c);
    }
    for c in ["MYCALL N1CALL", "BW2300", "CHAT OFF", "LISTEN ON"] {
        b.send(c);
    }
    lab.settle();
    a.send("CONNECT N0CALL N1CALL");
    let (al, bl) = (a.lines.clone(), b.lines.clone());
    let want = "CONNECTED N0CALL N1CALL 2300";
    let has = |l: &common::Lines| l.lock().unwrap().iter().any(|x| x == want);
    assert!(lab.run_until(30.0, |_| has(&al) && has(&bl)), "A: {:?}\nB: {:?}", a.since(0), b.since(0));
}

#[test]
fn linked_replies_and_deferred_mycall() {
    let mut lab = Lab::new(2);
    let (mut a, mut b) = (lab.attach(0), lab.attach(1));
    link(&mut lab, &mut a, &mut b);
    lab.run_until(8.0, |_| false);
    let last = ["LISTEN ON", "LISTEN OFF", "DISCONNECT", "ABORT", "MYCALL N0CALL-5", "CLEANTXBUFFER"];
    for v in vector("host/commands.json")["vectors"].as_array().unwrap() {
        let line = v["line"].as_str().unwrap();
        if v["state"] != "linked" || last.contains(&line) {
            continue;
        }
        let want: Vec<String> = v["expect"].as_array().unwrap().iter().map(|e| e.as_str().unwrap().into()).collect();
        let got = ask(&mut lab, &mut a, format!("{line}\r").as_bytes(), want.len());
        assert_eq!(got, want, "{line}");
        assert_ne!(lab.tnc[0].station().state(), State::Listen, "{line}");
    }
    assert_eq!(lab.tnc[0].station().bandwidth(), xara_phy::hf::level::Bandwidth::Hz2300);
    assert_eq!(ask(&mut lab, &mut a, b"CLEANTXBUFFER\r", 1), ["CLEANTXBUFFER BUFFEREMPTY"]);
    assert_eq!(ask(&mut lab, &mut a, b"MYCALL N0CALL-5\r", 1), ["WRONG"]);
    assert_eq!(ask(&mut lab, &mut b, b"MYCALL N1CALL-0\r", 1), ["WRONG"]);
    let start = a.len();
    assert_eq!(ask(&mut lab, &mut a, b"LISTEN ON\r", 1), ["OK"]);
    let al = a.lines.clone();
    assert!(lab.run_until(10.0, |_| al.lock().unwrap().iter().any(|l| l == "DISCONNECTED")));
    let tail: Vec<String> = a.since(start).into_iter().filter(|l| !l.starts_with("BUSY")).collect();
    assert_eq!(tail, ["OK", "PTT ON", "PTT OFF", "DISCONNECTED"]);
    assert!(
        lab.run_until(6.0, |_| al.lock().unwrap()[start..].iter().any(|l| l == "REGISTERED")),
        "{:?}",
        a.since(start)
    );
    assert_eq!(lab.tnc[0].station().config().host.mycalls, ["N0CALL-5"]);
    assert_eq!(a.count("OK"), 10, "{:?}", a.since(0));
    let bl = b.lines.clone();
    assert!(lab.run_until(90.0, |_| bl.lock().unwrap().iter().any(|l| l == "DISCONNECTED")), "{:?}", b.since(0));
    let bt = b.since(0);
    let i = bt.iter().position(|l| l == "DISCONNECTED").unwrap();
    assert_eq!(bt[i - 1], "WRONG");
    assert_eq!(lab.tnc[1].station().config().host.mycalls, ["N1CALL"]);
}

fn scenario(name: &str) -> Vec<(String, Vec<String>)> {
    events(&vector("host/messages.json")["sequences"][name])
}

#[test]
fn session_order_matches_vara() {
    let mut lab = Lab::new(2);
    let (mut a, mut b) = (lab.attach(0), lab.attach(1));
    lab.settle();
    let (a0, b0) = (a.len(), b.len());
    link(&mut lab, &mut a, &mut b);
    let msg: Vec<u8> = (0..1500u32).map(|i| (i * 7 % 251) as u8).collect();
    a.write(&msg);
    let bb = b.bytes.clone();
    assert!(lab.run_until(400.0, |_| bb.lock().unwrap().len() >= msg.len()), "{:?}", a.since(0));
    assert!(lab.run_until(30.0, |l| l.tnc[0].station().state() == State::Idle));
    a.send("DISCONNECT");
    let (al, bl) = (a.lines.clone(), b.lines.clone());
    let gone = |v: &common::Lines| v.lock().unwrap().iter().any(|l| l == "DISCONNECTED");
    assert!(lab.run_until(40.0, |_| gone(&al) && gone(&bl)));
    lab.run_until(3.0, |_| false);
    let (at, bt) = (a.since(a0), b.since(b0));
    let at: Vec<String> = at.into_iter().filter(|l| !is_reply(l) || l == "OK").collect();
    let at: Vec<String> = at[at.iter().position(|l| l.starts_with("PTT")).unwrap() - 1..].to_vec();
    check_sequence("caller", &scenario("caller_session_chatoff"), &at, &[]);
    check_sequence("answerer", &scenario("answerer_session_chatoff"), &bt, &[]);
    let sn = bt.iter().filter(|l| l.starts_with("SN ")).count();
    let rx = bt.iter().filter(|l| l.starts_with("BITRATE") && l.ends_with("RX")).count();
    assert!(rx > sn && sn > 3, "{sn} {rx}");
    assert!(at.iter().all(|l| !l.starts_with("LINK ")));
    assert_eq!(bt.iter().filter(|l| l.starts_with("LINK ")).count(), 1);
    let buffers: Vec<usize> = at.iter().filter_map(|l| l.strip_prefix("BUFFER ")).map(|n| n.parse().unwrap()).collect();
    assert_eq!(buffers.last(), Some(&0));
    assert!(buffers.windows(2).all(|w| w[1] < w[0]), "{buffers:?}");
    for t in [&at, &bt] {
        let i = t.iter().rposition(|l| l == "DISCONNECTED").unwrap();
        assert_eq!(t[i - 1], "PTT OFF");
        assert_eq!(t[i + 1..].iter().filter(|l| l.starts_with("BUSY")).cloned().collect::<Vec<_>>(), ["BUSY OFF"]);
    }
}

#[test]
fn failed_call_and_failed_incoming() {
    let mut lab = Lab::new(2);
    let (mut a, mut b) = (lab.attach(0), lab.attach(1));
    lab.settle();
    for c in ["MYCALL N0CALL", "BW2300", "RETRIES 2"] {
        a.send(c);
    }
    lab.settle();
    let a0 = a.len();
    a.send("CONNECT N0CALL N1CALL");
    let al = a.lines.clone();
    let gone = |v: &common::Lines| v.lock().unwrap().iter().any(|l| l == "DISCONNECTED");
    assert!(lab.run_until(30.0, |_| gone(&al)));
    let t0 = lab.now();
    assert!(lab.run_until(3.0, |l| !l.tnc[0].station().busy()));
    let hang = lab.now() - t0;
    assert!((1.40..1.52).contains(&hang), "{hang}");
    lab.settle();
    let at = a.since(a0);
    assert_eq!(at[..3], ["OK", "PTT ON", "BUSY ON"]);
    check_sequence("failed_call", &scenario("failed_call"), &at, &[]);
    assert_eq!(at.iter().filter(|l| *l == "PTT ON").count(), 3, "{at:?}");

    for c in ["MYCALL N1CALL", "BW2300", "LISTEN ON"] {
        b.send(c);
    }
    a.send("RETRIES 1");
    lab.settle();
    lab.deaf[0] = true;
    let b0 = b.len();
    a.send("CONNECT N0CALL N1CALL");
    let bl = b.lines.clone();
    assert!(lab.run_until(40.0, |_| bl.lock().unwrap().iter().any(|l| l == "CANCELPENDING")), "{:?}", b.since(b0));
    lab.run_until(3.0, |_| false);
    let bt = b.since(b0);
    check_sequence("failed_incoming", &scenario("failed_incoming"), &bt, &[]);
    let i = bt.iter().position(|l| l == "CANCELPENDING").unwrap();
    assert!(!bt.iter().any(|l| l == "DISCONNECTED" || l.starts_with("CONNECTED")), "{bt:?}");
    assert!(!bt[i..].iter().any(|l| l.starts_with("PTT")), "{bt:?}");
}

#[test]
fn registered_and_iamalive() {
    let mut lab = Lab::new(1);
    let mut c = lab.attach(0);
    lab.settle();
    for l in ["MYCALL N0CALL", "MYCALL N0CALL-0", "MYCALL N0CALL-1"] {
        c.send(l);
        lab.run_until(1.0, |_| false);
    }
    let t = lab.now();
    let cl = c.lines.clone();
    assert!(lab.run_until(8.0, |_| cl.lock().unwrap().iter().any(|l| l == "REGISTERED")));
    assert!((3.9..4.1).contains(&(lab.now() - t)), "{}", lab.now() - t);
    lab.settle();
    let tail = c.since(0);
    let i = tail.iter().position(|l| l == "REGISTERED").unwrap();
    assert_eq!(tail[i + 1], "ENCRYPTION DISABLED");
    assert_eq!(c.count("REGISTERED"), 1);
    assert!(lab.run_until(60.0, |_| cl.lock().unwrap().iter().any(|l| l == "IAMALIVE")));
    assert!((59.9..60.1).contains(&lab.now()), "{}", lab.now());
}

#[test]
fn registered_station_lists_its_calls() {
    let mut cfg = Config::new(Bandwidth::Hz500, "XX0A");
    cfg.host.registered = true;
    let tnc = Tnc::new(Station::new(cfg, common::options()), Host::bind("127.0.0.1", 0).unwrap());
    let mut lab = Lab::with(vec![tnc]);
    let mut c = lab.attach(0);
    lab.settle();
    assert_eq!(ask(&mut lab, &mut c, b"MYCALL N0CALL N0CALL-5\r", 1), ["OK"]);
    let cl = c.lines.clone();
    assert!(lab.run_until(8.0, |_| cl.lock().unwrap().iter().any(|l| l.starts_with("REGISTERED"))));
    lab.settle();
    let tail = c.since(0);
    let i = tail.iter().position(|l| l.starts_with("REGISTERED")).unwrap();
    assert_eq!(tail[i..i + 2], ["REGISTERED N0CALL N0CALL-5", "ENCRYPTION DISABLED"]);
}

#[test]
fn one_client_per_port_and_drop_resets() {
    let mut lab = Lab::new(2);
    let (mut a, mut b) = (lab.attach(0), lab.attach(1));
    lab.settle();
    for addr in [lab.tnc[0].host().cmd_addr, lab.tnc[0].host().data_addr] {
        assert!(turned_away(addr));
    }
    assert_eq!(ask(&mut lab, &mut a, b"VERSION\r", 1), ["VERSION VARA HF v4.9.0"]);

    b.write(b"queued before the link");
    link(&mut lab, &mut a, &mut b);
    let ab = a.bytes.clone();
    assert!(lab.run_until(120.0, |_| ab.lock().unwrap().len() >= 22));
    assert_eq!(&a.bytes.lock().unwrap()[..], b"queued before the link");
    lab.run_until(5.0, |_| false);

    let _ = a.data.shutdown(std::net::Shutdown::Both);
    let al = a.lines.clone();
    lab.run_until(1.0, |_| false);
    let n = al.lock().unwrap().len();
    let mut probe = [0u8; 1];
    a.cmd.set_read_timeout(Some(std::time::Duration::from_millis(500))).unwrap();
    assert_eq!(std::io::Read::read(&mut a.cmd.try_clone().unwrap(), &mut probe).unwrap_or(0), 0);
    assert!(lab.run_until(10.0, |l| l.tnc[0].station().state() == State::Listen));
    assert_eq!(al.lock().unwrap().len(), n);
    let cfg = lab.tnc[0].station().config().clone();
    assert!(!cfg.host.chat);
    let bl = b.lines.clone();
    assert!(!bl.lock().unwrap().iter().any(|l| l == "DISCONNECTED"));
    assert!(lab.run_until(90.0, |_| bl.lock().unwrap().iter().any(|l| l == "DISCONNECTED")));

    assert!(lab.run_until(5.0, |l| !l.tnc[0].station().busy()));
    let mut a2 = lab.attach(0);
    lab.settle();
    assert_eq!(a2.since(0)[..2], ["BUSY ON", "BUSY OFF"]);
    a2.write(b"lost at link end");
    assert!(lab.wait_for(|l| l.tnc[0].station().buffered() == 16));
    for c in ["RETRIES 1", "LISTEN ON"] {
        a2.send(c);
    }
    b.send("LISTEN OFF");
    lab.settle();
    a2.send("CONNECT N0CALL N1CALL");
    let a2l = a2.lines.clone();
    assert!(lab.run_until(30.0, |_| a2l.lock().unwrap().iter().any(|l| l == "DISCONNECTED")));
    assert_eq!(ask(&mut lab, &mut a2, b"CLEANTXBUFFER\r", 1), ["CLEANTXBUFFER BUFFEREMPTY"]);
    assert!(!a2.since(0).iter().any(|l| l.starts_with("BUFFER")));
}

#[test]
fn forced_ptt_release_reports_once() {
    let mut lab = Lab::new(1);
    let mut a = lab.attach(0);
    lab.settle();
    a.send("IGNOREKISSDCD ON");
    a.send("CQFRAME N0CALL 2300");
    assert!(lab.run_until(5.0, |l| l.tnc[0].keyed()), "{:?}", a.since(0));
    lab.settle();
    let start = a.len();
    lab.tnc[0].force_ptt_off(true);
    assert!(lab.run_until(15.0, |l| !l.tnc[0].keyed()));
    lab.tnc[0].force_ptt_off(false);
    lab.settle();
    let ptt: Vec<String> = a.since(start).into_iter().filter(|l| l.starts_with("PTT")).collect();
    assert_eq!(ptt, ["PTT OFF"]);
}

#[test]
fn tune_and_drivelevel_follow_vara() {
    let (mut lab, mut c) = solo();
    let drive = |l: &Lab| l.tnc[0].station().options().drive_db;
    let tuning = |l: &Lab| l.tnc[0].station().tuning();
    assert_eq!(ask(&mut lab, &mut c, b"TUNE -40\r", 1), ["OK"]);
    assert!(!tuning(&lab) && drive(&lab) == -5.0);
    assert_eq!(ask(&mut lab, &mut c, b"DRIVELEVEL -12\r", 1), ["OK"]);
    assert!(!tuning(&lab));
    assert_eq!(ask(&mut lab, &mut c, b"TUNE ?\r", 1), ["TUNE -12"]);
    for junk in [&b"DRIVELEVEL abc\r"[..], b"DRIVELEVEL 3\r"] {
        assert_eq!(ask(&mut lab, &mut c, junk, 1), ["OK"]);
    }
    assert_eq!(drive(&lab), -12.0);
    assert_eq!(ask(&mut lab, &mut c, b"TUNE ON\r", 1), ["OK"]);
    assert!(tuning(&lab) && drive(&lab) == -12.0 && c.saw("PTT ON"));
    assert_eq!(ask(&mut lab, &mut c, b"TUNE -8\r", 1), ["OK"]);
    assert!(tuning(&lab) && drive(&lab) == -8.0);
    assert_eq!(ask(&mut lab, &mut c, b"DRIVELEVEL -9.5\r", 1), ["OK"]);
    assert!(tuning(&lab) && drive(&lab) == -10.0);
    assert_eq!(ask(&mut lab, &mut c, b"TUNE OFF\r", 1), ["OK"]);
    assert!(!tuning(&lab) && c.saw("PTT OFF"));
    assert_eq!(ask(&mut lab, &mut c, b"TUNE OFF\r", 1), ["OK"]);
    assert_eq!(ask(&mut lab, &mut c, b"TUNE\r", 1), ["WRONG"]);
    assert_eq!(ask(&mut lab, &mut c, b"tune -5\r", 1), ["WRONG"]);

    assert_eq!(ask(&mut lab, &mut c, b"TUNE -5.5\r", 1), ["OK"]);
    assert!(tuning(&lab) && drive(&lab) == -6.0);
    let t = lab.now();
    assert!(lab.run_until(70.0, |l| !l.tnc[0].station().tuning()));
    assert!((lab.now() - t - 60.0).abs() < 0.2, "{}", lab.now() - t);
}

#[test]
fn a_host_tune_stops_when_the_host_goes() {
    let (mut lab, mut c) = solo();
    assert_eq!(ask(&mut lab, &mut c, b"TUNE -10\r", 1), ["OK"]);
    assert!(lab.tnc[0].station().tuning());
    c.close();
    assert!(lab.run_until(2.0, |l| !l.tnc[0].station().tuning()));
    assert!(lab.run_until(2.0, |l| !l.tnc[0].keyed()));
}

#[test]
fn tune_while_calling_only_sets_the_drive() {
    let (mut lab, mut c) = solo();
    assert_eq!(ask(&mut lab, &mut c, b"CONNECT N0CALL N1CALL\r", 1), ["OK"]);
    assert_eq!(lab.tnc[0].station().state(), State::Calling);
    assert_eq!(ask(&mut lab, &mut c, b"TUNE -7\r", 1), ["OK"]);
    assert!(!lab.tnc[0].station().tuning());
    assert_eq!(lab.tnc[0].station().options().drive_db, -7.0);
}
