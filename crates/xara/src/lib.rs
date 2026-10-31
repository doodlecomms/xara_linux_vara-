#[macro_use]
pub mod log;
pub mod audio;
#[cfg(target_os = "macos")]
mod cf;
pub mod cli;
pub mod flac;
#[cfg(target_os = "macos")]
mod hid;
mod hidraw;
pub mod monitor;
pub mod ptt;
pub mod record;
pub mod resample;
pub mod signals;
pub mod station;
pub mod tnc;
pub mod wav;

pub const DEFAULT_ADDR: &str = "127.0.0.1";
pub const HF_SPAN_HZ: u32 = 3000;
pub const FM_SPAN_HZ: u32 = 6000;

#[cfg(target_os = "linux")]
pub(crate) type Ioctl = libc::Ioctl;
#[cfg(not(target_os = "linux"))]
pub(crate) type Ioctl = libc::c_ulong;

pub(crate) fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
