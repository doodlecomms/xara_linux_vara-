use std::io::{ErrorKind, Read};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

pub fn turned_away(addr: SocketAddr) -> bool {
    match TcpStream::connect(addr) {
        Err(e) => {
            e.kind() == ErrorKind::ConnectionRefused
                || !cfg!(target_os = "linux") && e.kind() == ErrorKind::ConnectionReset
        }
        Ok(_) if cfg!(target_os = "linux") => false,
        Ok(s) => hung_up(s),
    }
}

pub fn hung_up(mut s: TcpStream) -> bool {
    if let Err(e) = s.set_read_timeout(Some(Duration::from_secs(1))) {
        return e.kind() == ErrorKind::InvalidInput;
    }
    match s.read(&mut [0]) {
        Ok(n) => n == 0,
        Err(e) => e.kind() == ErrorKind::ConnectionReset,
    }
}
