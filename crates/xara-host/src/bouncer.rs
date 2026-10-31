use std::io;
use std::net::{TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::sync::{Arc, Mutex};
use std::thread;

use crate::server::ACCEPT_BACKOFF;

const POLL_MS: libc::c_int = 1000;

struct Gate {
    l: TcpListener,
    stop: Mutex<bool>,
}

pub(crate) struct Bouncer(Arc<Gate>);

impl Bouncer {
    pub(crate) fn start(l: &TcpListener) -> io::Result<Self> {
        let g = Arc::new(Gate { l: l.try_clone()?, stop: Mutex::default() });
        g.l.set_nonblocking(true)?;
        let b = Self(g.clone());
        thread::Builder::new().name("host-bouncer".into()).spawn(move || bounce(&g))?;
        Ok(b)
    }

    pub(crate) fn resume(&mut self, _: &TcpListener) -> io::Result<()> {
        self.stop()
    }

    fn stop(&self) -> io::Result<()> {
        let mut stop = self.0.stop.lock().unwrap();
        *stop = true;
        self.0.l.set_nonblocking(false)
    }
}

impl Drop for Bouncer {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn bounce(g: &Gate) {
    loop {
        let ready = pending(&g.l);
        let stop = g.stop.lock().unwrap();
        if *stop {
            return;
        }
        if !ready {
            continue;
        }
        match g.l.accept() {
            Ok((c, _)) => reset(&c),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(_) => {
                drop(stop);
                thread::sleep(ACCEPT_BACKOFF);
            }
        }
    }
}

fn pending(l: &TcpListener) -> bool {
    let mut p = libc::pollfd { fd: l.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    // SAFETY: p is one valid pollfd for the duration of the call.
    unsafe { libc::poll(&mut p, 1, POLL_MS) > 0 }
}

fn reset(c: &TcpStream) {
    let linger = libc::linger { l_onoff: 1, l_linger: 0 };
    let len = std::mem::size_of_val(&linger) as libc::socklen_t;
    // SAFETY: c is an open socket and linger outlives the call.
    unsafe { libc::setsockopt(c.as_raw_fd(), libc::SOL_SOCKET, libc::SO_LINGER, (&raw const linger).cast(), len) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpStream;
    use std::time::Duration;
    use xara_testkit::net::hung_up;

    #[test]
    fn a_bouncer_turns_clients_away_until_dropped() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let a = l.local_addr().unwrap();
        let _first = TcpStream::connect(a).unwrap();
        drop(l.accept().unwrap());
        let b = Bouncer::start(&l).unwrap();
        for _ in 0..3 {
            match TcpStream::connect(a) {
                Ok(s) => assert!(hung_up(s)),
                Err(e) => assert_eq!(e.kind(), io::ErrorKind::ConnectionReset),
            }
        }
        drop(b);
        let next = TcpStream::connect(a).unwrap();
        assert_eq!(l.accept().unwrap().1, next.local_addr().unwrap());
        l.set_nonblocking(true).unwrap();
        assert_eq!(l.accept().unwrap_err().kind(), io::ErrorKind::WouldBlock);
    }

    #[test]
    fn a_client_arriving_while_the_bouncer_stops_is_served() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let a = l.local_addr().unwrap();
        let mut b = Bouncer::start(&l).unwrap();
        thread::sleep(Duration::from_millis(20));
        let mut stop = b.0.stop.lock().unwrap();
        let c = TcpStream::connect(a).unwrap();
        thread::sleep(Duration::from_millis(50));
        *stop = true;
        drop(stop);
        b.resume(&l).unwrap();
        assert!(!hung_up(c.try_clone().unwrap()));
        assert_eq!(l.accept().unwrap().1, c.local_addr().unwrap());
    }
}
