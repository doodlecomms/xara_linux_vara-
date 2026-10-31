use std::ffi::c_void;
use std::io;
use std::ptr::null;

use crate::cf::{Cf, CfIndex};
use crate::ptt::{Ptt, cm108_report};

const REPORT_OUTPUT: u32 = 1;

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOHIDManagerCreate(alloc: *const c_void, options: u32) -> *const c_void;
    fn IOHIDManagerSetDeviceMatching(m: *const c_void, matching: *const c_void);
    fn IOHIDManagerOpen(m: *const c_void, options: u32) -> i32;
    fn IOHIDManagerCopyDevices(m: *const c_void) -> *const c_void;
    fn IOHIDDeviceSetReport(d: *const c_void, kind: u32, id: CfIndex, report: *const u8, len: CfIndex) -> i32;
}

struct HidDevice {
    dev: Cf,
    _manager: Cf,
}

// SAFETY: whichever thread owns it is its only user, and unscheduled IOKit HID refs may be used and freed anywhere.
unsafe impl Send for HidDevice {}

impl HidDevice {
    fn open(vendor: u16, product: u16) -> io::Result<Self> {
        let ids = format!("{vendor:04x}:{product:04x}");
        let (kv, kp) = (Cf::string("VendorID")?, Cf::string("ProductID")?);
        let (vv, vp) = (Cf::number(vendor.into())?, Cf::number(product.into())?);
        let matching = Cf::dictionary(&[(&kv, &vv), (&kp, &vp)])?;
        // SAFETY: a null allocator is the default one, no options are set, and Create hands over one reference.
        let manager = unsafe { Cf::created(IOHIDManagerCreate(null(), 0), "IOHIDManagerCreate")? };
        // SAFETY: manager and matching are live; the manager copies the matching dictionary.
        unsafe { IOHIDManagerSetDeviceMatching(manager.ptr(), matching.ptr()) };
        let none = || io::Error::other(format!("no USB HID device {ids} found"));
        // SAFETY: manager is live; the copied set comes back with one reference, or null when none match.
        let set = unsafe { Cf::owned(IOHIDManagerCopyDevices(manager.ptr())) }.ok_or_else(none)?;
        // SAFETY: set is the CFSet just copied.
        let dev = match unsafe { set.set_values() }[..] {
            [d] => d,
            [] => return Err(none()),
            ref all => {
                return Err(io::Error::other(format!("{} USB HID devices match {ids}; unplug all but one", all.len())));
            }
        };
        // SAFETY: manager is live and matches the one device in set.
        let r = unsafe { IOHIDManagerOpen(manager.ptr(), 0) };
        if r != 0 {
            return Err(io::Error::other(format!("opening USB HID device {ids}: IOKit error {r:#x}")));
        }
        // SAFETY: dev is a live device that set keeps until here.
        let dev = unsafe { Cf::retained(dev)? };
        Ok(Self { dev, _manager: manager })
    }

    fn write(&mut self, report: &[u8]) -> io::Result<()> {
        let (id, data) = report.split_first().ok_or_else(|| io::Error::other("empty HID report"))?;
        // SAFETY: the device is live and data is valid for its length.
        let r = unsafe {
            IOHIDDeviceSetReport(
                self.dev.ptr(),
                REPORT_OUTPUT,
                CfIndex::from(*id),
                data.as_ptr(),
                data.len() as CfIndex,
            )
        };
        if r != 0 {
            return Err(io::Error::other(format!("HID output report: IOKit error {r:#x}")));
        }
        Ok(())
    }
}

pub fn cm108_usb(vendor: u16, product: u16, gpio: u8) -> io::Result<Box<dyn Ptt>> {
    let mut c = Cm108Usb { dev: HidDevice::open(vendor, product)?, id: (vendor, product), gpio };
    c.set(false)?;
    Ok(Box::new(c))
}

struct Cm108Usb {
    dev: HidDevice,
    id: (u16, u16),
    gpio: u8,
}

impl Ptt for Cm108Usb {
    fn set(&mut self, on: bool) -> io::Result<()> {
        self.dev.write(&cm108_report(self.gpio, on))
    }

    fn describe(&self) -> String {
        format!("CM108 GPIO{} on USB {:04x}:{:04x}", self.gpio, self.id.0, self.id.1)
    }
}
