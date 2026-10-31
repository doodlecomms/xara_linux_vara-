#[cfg(not(target_os = "macos"))]
use std::io;

#[cfg(target_os = "freebsd")]
use crate::Ioctl;

#[cfg(target_os = "linux")]
pub fn is_hidraw(path: &str) -> bool {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let Ok(m) = std::fs::metadata(path) else { return false };
    if !m.file_type().is_char_device() {
        return false;
    }
    let (major, minor) = (libc::major(m.rdev()), libc::minor(m.rdev()));
    std::fs::read_link(format!("/sys/dev/char/{major}:{minor}/subsystem"))
        .is_ok_and(|l| l.file_name().is_some_and(|n| n == "hidraw"))
}

#[cfg(target_os = "freebsd")]
pub fn is_hidraw(path: &str) -> bool {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let Ok(m) = std::fs::metadata(path) else { return false };
    if !m.file_type().is_char_device() {
        return false;
    }
    let mut buf = [0 as libc::c_char; 64];
    // SAFETY: buf is valid for buf.len() bytes and devname_r NUL-terminates what it returns.
    let p = unsafe { libc::devname_r(m.rdev() as libc::dev_t, libc::S_IFCHR, buf.as_mut_ptr(), buf.len() as _) };
    // SAFETY: p is null or points at a NUL-terminated name in buf or in static storage.
    !p.is_null() && unsafe { std::ffi::CStr::from_ptr(p) }.to_bytes().starts_with(b"hidraw")
}

#[cfg(not(hidraw))]
pub fn is_hidraw(_: &str) -> bool {
    false
}

#[cfg(target_os = "freebsd")]
#[repr(C)]
#[derive(Default)]
struct HidrawDevinfo {
    bustype: u32,
    vendor: i16,
    product: i16,
}

#[cfg(target_os = "freebsd")]
const HIDIOCGRAWINFO: Ioctl = libc::_IOR::<HidrawDevinfo>(b'U' as _, 32);

#[cfg(hidraw)]
pub fn find(vendor: u16, product: u16) -> io::Result<String> {
    let mut nodes: Vec<String> = std::fs::read_dir("/dev")?
        .flatten()
        .filter_map(|e| e.file_name().to_str().filter(|n| n.starts_with("hidraw")).map(|n| format!("/dev/{n}")))
        .collect();
    nodes.sort_by(|a, b| a.len().cmp(&b.len()).then(a.cmp(b)));
    let (mut found, mut failed) = (Vec::new(), Vec::new());
    for p in nodes {
        match usb_id(&p) {
            Ok(id) if id == (vendor, product) => found.push(p),
            Ok(_) => {}
            Err(e) => failed.push(format!("{p}: {e}")),
        }
    }
    pick(&format!("{vendor:04x}:{product:04x}"), found, &failed)
}

#[cfg(not(any(hidraw, target_os = "macos")))]
pub fn find(vendor: u16, product: u16) -> io::Result<String> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        format!("--ptt cm108:{vendor:04x}:{product:04x}: not supported here yet"),
    ))
}

#[cfg(target_os = "linux")]
fn usb_id(path: &str) -> io::Result<(u16, u16)> {
    let name = path.trim_start_matches("/dev/");
    let uevent = std::fs::read_to_string(format!("/sys/class/hidraw/{name}/device/uevent"))?;
    uevent
        .lines()
        .find_map(|l| l.strip_prefix("HID_ID="))
        .and_then(hid_id)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no HID_ID in its uevent"))
}

#[cfg(target_os = "linux")]
fn hid_id(s: &str) -> Option<(u16, u16)> {
    let mut f = s.split(':').skip(1).map(|h| u32::from_str_radix(h, 16).ok().and_then(|v| u16::try_from(v).ok()));
    Some((f.next()??, f.next()??))
}

#[cfg(target_os = "freebsd")]
fn usb_id(path: &str) -> io::Result<(u16, u16)> {
    use std::os::fd::AsRawFd;
    let f = std::fs::File::open(path)?;
    let mut info = HidrawDevinfo::default();
    // SAFETY: HIDIOCGRAWINFO writes one hidraw_devinfo, which HidrawDevinfo matches in layout.
    if unsafe { libc::ioctl(f.as_raw_fd(), HIDIOCGRAWINFO, &mut info) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((info.vendor as u16, info.product as u16))
}

#[cfg(hidraw)]
fn pick(id: &str, mut found: Vec<String>, failed: &[String]) -> io::Result<String> {
    let unchecked = match failed.is_empty() {
        true => String::new(),
        false => format!("; could not check {}", failed.join(", ")),
    };
    match found.len() {
        0 => Err(io::Error::new(io::ErrorKind::NotFound, format!("no hidraw device has USB id {id}{unchecked}"))),
        1 => Ok(found.remove(0)),
        _ => Err(io::Error::other(format!(
            "USB id {id} matches {}{unchecked}; pick one with --ptt cm108:DEV",
            found.join(", ")
        ))),
    }
}

#[cfg(all(test, hidraw))]
mod tests {
    use super::*;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn one_match_is_picked_past_nodes_it_could_not_check() {
        assert_eq!(pick("0d8c:0012", v(&["/dev/hidraw1"]), &[]).unwrap(), "/dev/hidraw1");
        let denied = v(&["/dev/hidraw0: Permission denied (os error 13)"]);
        assert_eq!(pick("0d8c:0012", v(&["/dev/hidraw1"]), &denied).unwrap(), "/dev/hidraw1");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_usb_id_comes_from_the_uevent_hid_id() {
        assert_eq!(hid_id("0003:00001209:00007388"), Some((0x1209, 0x7388)));
        assert_eq!(hid_id("0003:00000D8C:00000012"), Some((0x0d8c, 0x0012)));
        assert_eq!(hid_id("0003:00001209"), None);
        assert_eq!(hid_id("0003:0001FFFF:00007388"), None);
    }

    #[test]
    fn no_match_names_the_nodes_it_could_not_check() {
        let e = pick("0d8c:0012", v(&[]), &[]).unwrap_err();
        assert_eq!(
            (e.kind(), e.to_string().as_str()),
            (io::ErrorKind::NotFound, "no hidraw device has USB id 0d8c:0012")
        );
        let failed =
            v(&["/dev/hidraw0: Permission denied (os error 13)", "/dev/hidraw2: No such device (os error 19)"]);
        let e = pick("0d8c:0012", v(&[]), &failed).unwrap_err().to_string();
        let want = "could not check /dev/hidraw0: Permission denied (os error 13), \
                    /dev/hidraw2: No such device (os error 19)";
        assert!(e.ends_with(want), "{e}");
    }

    #[test]
    fn several_matches_are_ambiguous() {
        let e = pick("0d8c:0012", v(&["/dev/hidraw1", "/dev/hidraw3"]), &[]).unwrap_err().to_string();
        assert_eq!(e, "USB id 0d8c:0012 matches /dev/hidraw1, /dev/hidraw3; pick one with --ptt cm108:DEV");
    }
}
