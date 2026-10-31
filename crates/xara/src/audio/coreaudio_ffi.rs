use std::ffi::c_void;
use std::io;
use std::ptr::{null, null_mut};
use std::slice;
use std::sync::Arc;

use crate::cf::Cf;

type OsStatus = i32;
type ObjectId = u32;
type QueueRef = *mut c_void;

const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}

const SYSTEM_OBJECT: ObjectId = 1;
const HARDWARE_DEVICES: u32 = fourcc(b"dev#");
const DEVICE_UID: u32 = fourcc(b"uid ");
const OBJECT_NAME: u32 = fourcc(b"lnam");
const DEVICE_STREAM_CONFIG: u32 = fourcc(b"slay");
const DEVICE_TRANSPORT: u32 = fourcc(b"tran");
const DEVICE_BUFFER_FRAMES: u32 = fourcc(b"fsiz");
const DEVICE_LATENCY: u32 = fourcc(b"ltnc");
const DEVICE_SAFETY_OFFSET: u32 = fourcc(b"saft");
const DEVICE_STREAMS: u32 = fourcc(b"stm#");
const DEVICE_RATE: u32 = fourcc(b"nsrt");
const DEVICE_IS_ALIVE: u32 = fourcc(b"livn");
const DEFAULT_INPUT: u32 = fourcc(b"dIn ");
const STREAM_LATENCY: u32 = fourcc(b"ltnc");
const SCOPE_GLOBAL: u32 = fourcc(b"glob");
const SCOPE_INPUT: u32 = fourcc(b"inpt");
const SCOPE_OUTPUT: u32 = fourcc(b"outp");
const ELEMENT_MAIN: u32 = 0;
pub const TRANSPORT_VIRTUAL: u32 = fourcc(b"virt");
pub const TRANSPORT_AGGREGATE: u32 = fourcc(b"grup");

const FORMAT_LINEAR_PCM: u32 = fourcc(b"lpcm");
const PCM_SIGNED_INTEGER: u32 = 1 << 2;
const PCM_PACKED: u32 = 1 << 3;
const QUEUE_CURRENT_DEVICE: u32 = fourcc(b"aqcd");
const QUEUE_CHANNEL_ASSIGNMENTS: u32 = fourcc(b"aqca");

#[repr(C)]
struct PropertyAddress {
    selector: u32,
    scope: u32,
    element: u32,
}

#[repr(C)]
struct ChannelAssignment {
    uid: *const c_void,
    channel: u32,
}

#[repr(C)]
pub struct StreamFormat {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    reserved: u32,
}

impl StreamFormat {
    pub fn s16_mono(rate: u32) -> Self {
        Self {
            sample_rate: f64::from(rate),
            format_id: FORMAT_LINEAR_PCM,
            format_flags: PCM_SIGNED_INTEGER | PCM_PACKED,
            bytes_per_packet: 2,
            frames_per_packet: 1,
            bytes_per_frame: 2,
            channels_per_frame: 1,
            bits_per_channel: 16,
            reserved: 0,
        }
    }
}

#[repr(C)]
struct QueueBuffer {
    capacity: u32,
    data: *mut c_void,
    size: u32,
    _user: *mut c_void,
    _desc_capacity: u32,
    _descs: *mut c_void,
    _desc_count: u32,
}

type InputCb = extern "C" fn(*mut c_void, QueueRef, *mut QueueBuffer, *const c_void, u32, *const c_void);
type OutputCb = extern "C" fn(*mut c_void, QueueRef, *mut QueueBuffer);

#[link(name = "CoreAudio", kind = "framework")]
unsafe extern "C" {
    fn AudioObjectGetPropertyDataSize(
        id: ObjectId,
        addr: *const PropertyAddress,
        qual_size: u32,
        qual: *const c_void,
        size: *mut u32,
    ) -> OsStatus;
    fn AudioObjectGetPropertyData(
        id: ObjectId,
        addr: *const PropertyAddress,
        qual_size: u32,
        qual: *const c_void,
        size: *mut u32,
        data: *mut c_void,
    ) -> OsStatus;
}

#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioQueueNewInput(
        format: *const StreamFormat,
        cb: InputCb,
        user: *mut c_void,
        run_loop: *const c_void,
        mode: *const c_void,
        flags: u32,
        out: *mut QueueRef,
    ) -> OsStatus;
    fn AudioQueueNewOutput(
        format: *const StreamFormat,
        cb: OutputCb,
        user: *mut c_void,
        run_loop: *const c_void,
        mode: *const c_void,
        flags: u32,
        out: *mut QueueRef,
    ) -> OsStatus;
    fn AudioQueueAllocateBuffer(q: QueueRef, bytes: u32, out: *mut *mut QueueBuffer) -> OsStatus;
    fn AudioQueueEnqueueBuffer(q: QueueRef, b: *mut QueueBuffer, ndesc: u32, descs: *const c_void) -> OsStatus;
    fn AudioQueueSetProperty(q: QueueRef, id: u32, data: *const c_void, size: u32) -> OsStatus;
    fn AudioQueueStart(q: QueueRef, at: *const c_void) -> OsStatus;
    fn AudioQueueDispose(q: QueueRef, immediate: u8) -> OsStatus;
}

fn status(what: &str, s: OsStatus) -> io::Result<()> {
    if s == 0 {
        return Ok(());
    }
    let b = s.to_be_bytes();
    let code = if b.iter().all(|c| c.is_ascii_graphic()) {
        format!("'{}'", String::from_utf8_lossy(&b))
    } else {
        s.to_string()
    };
    Err(io::Error::other(format!("{what}: CoreAudio error {code}")))
}

fn address(selector: u32, scope: u32) -> PropertyAddress {
    PropertyAddress { selector, scope, element: ELEMENT_MAIN }
}

fn property_size(id: ObjectId, selector: u32, scope: u32) -> io::Result<u32> {
    let mut size = 0;
    // SAFETY: the address outlives the call and size is writable.
    status("AudioObjectGetPropertyDataSize", unsafe {
        AudioObjectGetPropertyDataSize(id, &address(selector, scope), 0, null(), &mut size)
    })?;
    Ok(size)
}

fn property<T: Copy>(id: ObjectId, selector: u32, scope: u32, zero: T) -> io::Result<T> {
    let mut v = zero;
    let mut size = size_of::<T>() as u32;
    // SAFETY: v has room for size bytes and every caller asks for a plain-data property of type T.
    status("AudioObjectGetPropertyData", unsafe {
        AudioObjectGetPropertyData(id, &address(selector, scope), 0, null(), &mut size, (&raw mut v).cast())
    })?;
    Ok(v)
}

fn string_property(id: ObjectId, selector: u32) -> io::Result<String> {
    let p: *const c_void = property(id, selector, SCOPE_GLOBAL, null())?;
    // SAFETY: the UID and name properties are CFStrings handed over with a reference the caller releases.
    let s = unsafe { Cf::owned(p) }.ok_or_else(|| io::Error::other("CoreAudio returned no string"))?;
    s.utf8().ok_or_else(|| io::Error::other("CoreAudio returned a string that is not UTF-8"))
}

fn property_vec<T: Copy>(id: ObjectId, selector: u32, scope: u32, zero: T) -> io::Result<Vec<T>> {
    let size = property_size(id, selector, scope)? as usize;
    let mut v = vec![zero; size.div_ceil(size_of::<T>())];
    let mut got = (v.len() * size_of::<T>()) as u32;
    // SAFETY: v has room for got bytes, aligned for T, which every caller picks to suit the property.
    status("AudioObjectGetPropertyData", unsafe {
        AudioObjectGetPropertyData(id, &address(selector, scope), 0, null(), &mut got, v.as_mut_ptr().cast())
    })?;
    v.truncate((got as usize).div_ceil(size_of::<T>()));
    Ok(v)
}

fn channels(id: ObjectId, scope: u32) -> io::Result<u32> {
    let w = property_vec(id, DEVICE_STREAM_CONFIG, scope, 0u32)?;
    let short = || io::Error::other("CoreAudio returned a short stream configuration");
    (0..*w.first().ok_or_else(short)? as usize).map(|i| w.get(2 + 4 * i).copied().ok_or_else(short)).sum()
}

fn latency(id: ObjectId, scope: u32) -> u32 {
    let get = |id, selector, scope| property(id, selector, scope, 0u32).unwrap_or(0);
    let stream = property_vec(id, DEVICE_STREAMS, scope, 0 as ObjectId).ok().and_then(|v| v.first().copied());
    get(id, DEVICE_LATENCY, scope)
        + get(id, DEVICE_SAFETY_OFFSET, scope)
        + stream.map_or(0, |s| get(s, STREAM_LATENCY, SCOPE_GLOBAL))
}

pub fn default_input() -> io::Result<u32> {
    property(SYSTEM_OBJECT, DEFAULT_INPUT, SCOPE_GLOBAL, 0)
}

pub fn alive(id: u32) -> bool {
    property(id, DEVICE_IS_ALIVE, SCOPE_GLOBAL, 0u32).is_ok_and(|v| v != 0)
}

#[derive(Debug)]
pub struct DeviceInfo {
    pub id: u32,
    pub uid: String,
    pub name: String,
    pub inputs: u32,
    pub outputs: u32,
    pub transport: u32,
    pub io_frames: usize,
    pub in_latency: usize,
    pub out_latency: usize,
    pub rate: f64,
}

pub fn devices() -> io::Result<Vec<DeviceInfo>> {
    let ids = property_vec(SYSTEM_OBJECT, HARDWARE_DEVICES, SCOPE_GLOBAL, 0 as ObjectId)
        .map_err(|e| io::Error::other(format!("listing audio devices: {e}")))?;
    let mut v = Vec::new();
    for id in ids {
        let Ok(uid) = string_property(id, DEVICE_UID) else { continue };
        let (Ok(inputs), Ok(outputs)) = (channels(id, SCOPE_INPUT), channels(id, SCOPE_OUTPUT)) else { continue };
        v.push(DeviceInfo {
            name: string_property(id, OBJECT_NAME).unwrap_or_default(),
            inputs,
            outputs,
            transport: property(id, DEVICE_TRANSPORT, SCOPE_GLOBAL, 0u32).unwrap_or(0),
            io_frames: property(id, DEVICE_BUFFER_FRAMES, SCOPE_GLOBAL, 0u32).unwrap_or(0) as usize,
            in_latency: if inputs > 0 { latency(id, SCOPE_INPUT) as usize } else { 0 },
            out_latency: if outputs > 0 { latency(id, SCOPE_OUTPUT) as usize } else { 0 },
            rate: property(id, DEVICE_RATE, SCOPE_GLOBAL, 0f64).unwrap_or(0.0),
            id,
            uid,
        });
    }
    Ok(v)
}

pub trait Capture: Sync {
    fn captured(&self, x: &[i16]);
    fn rx_lost(&self);
}

pub trait Playback: Sync {
    fn refill(&self, played: usize, out: &mut [i16]);
    fn tx_lost(&self, frames: usize);
}

extern "C" fn captured<T: Capture>(
    user: *mut c_void,
    q: QueueRef,
    b: *mut QueueBuffer,
    _: *const c_void,
    _: u32,
    _: *const c_void,
) {
    // SAFETY: the AudioQueue owning q keeps user's T alive past its last callback; b has size bytes till re-enqueued.
    unsafe {
        let buf = &*b;
        let user = &*user.cast::<T>();
        user.captured(slice::from_raw_parts(buf.data.cast::<i16>(), buf.size as usize / 2));
        if AudioQueueEnqueueBuffer(q, b, 0, null()) != 0 {
            user.rx_lost();
        }
    }
}

extern "C" fn drained<T: Playback>(user: *mut c_void, q: QueueRef, b: *mut QueueBuffer) {
    // SAFETY: as in captured; every buffer goes out full, so the one coming back played all of its capacity.
    unsafe {
        let (user, frames) = (&*user.cast::<T>(), (*b).capacity as usize / 2);
        if refill(user, q, b, frames).is_err() {
            user.tx_lost(frames);
        }
    }
}

unsafe fn refill<T: Playback>(user: &T, q: QueueRef, b: *mut QueueBuffer, played: usize) -> io::Result<()> {
    // SAFETY: the caller lends b, a buffer of the live queue q, until it is enqueued again; data holds capacity bytes.
    unsafe {
        let buf = &mut *b;
        let out = slice::from_raw_parts_mut(buf.data.cast::<i16>(), buf.capacity as usize / 2);
        user.refill(played, out);
        buf.size = buf.capacity;
        status("AudioQueueEnqueueBuffer", AudioQueueEnqueueBuffer(q, b, 0, null()))
    }
}

pub struct AudioQueue<T> {
    raw: QueueRef,
    user: Arc<T>,
}

impl<T> Drop for AudioQueue<T> {
    fn drop(&mut self) {
        // SAFETY: raw is ours; immediate disposal returns after the last callback, and only then does self.user drop.
        unsafe { AudioQueueDispose(self.raw, 1) };
    }
}

impl<T> AudioQueue<T> {
    fn context(user: &Arc<T>) -> *mut c_void {
        Arc::as_ptr(user).cast_mut().cast()
    }

    fn set_device(&self, uid: &str, device_channels: u32) -> io::Result<()> {
        let s = Cf::string(uid)?;
        let p = s.ptr();
        // SAFETY: the queue is live and the value is a CFStringRef, which the queue retains.
        status(&format!("selecting {uid:?}"), unsafe {
            AudioQueueSetProperty(self.raw, QUEUE_CURRENT_DEVICE, (&raw const p).cast(), size_of_val(&p) as u32)
        })?;
        if device_channels <= 2 {
            return Ok(());
        }
        let a = ChannelAssignment { uid: p, channel: 1 };
        // SAFETY: the queue is live and stopped; a is one assignment for its one channel, and the queue retains the UID.
        status(&format!("channel 1 of {uid:?}"), unsafe {
            AudioQueueSetProperty(self.raw, QUEUE_CHANNEL_ASSIGNMENTS, (&raw const a).cast(), size_of_val(&a) as u32)
        })
    }

    fn allocate(&self, frames: usize) -> io::Result<*mut QueueBuffer> {
        let mut b = null_mut();
        // SAFETY: the queue is live; the buffer belongs to it and is freed when it is disposed.
        status("AudioQueueAllocateBuffer", unsafe { AudioQueueAllocateBuffer(self.raw, 2 * frames as u32, &mut b) })?;
        Ok(b)
    }

    pub fn start(&self) -> io::Result<()> {
        // SAFETY: the queue is live; a null start time means now.
        status("AudioQueueStart", unsafe { AudioQueueStart(self.raw, null()) })
    }
}

impl<T: Capture> AudioQueue<T> {
    pub fn input(
        format: &StreamFormat,
        device: &DeviceInfo,
        user: Arc<T>,
        buffers: usize,
        frames: usize,
    ) -> io::Result<Self> {
        let mut raw = null_mut();
        // SAFETY: format outlives the call and the context stays valid while the AudioQueue built below holds user.
        status("AudioQueueNewInput", unsafe {
            AudioQueueNewInput(format, captured::<T>, Self::context(&user), null(), null(), 0, &mut raw)
        })?;
        let q = Self { raw, user };
        q.set_device(&device.uid, device.inputs)?;
        for _ in 0..buffers {
            let b = q.allocate(frames)?;
            // SAFETY: b was just allocated from the live queue and is not enqueued yet.
            status("AudioQueueEnqueueBuffer", unsafe { AudioQueueEnqueueBuffer(q.raw, b, 0, null()) })?;
        }
        Ok(q)
    }
}

impl<T: Playback> AudioQueue<T> {
    pub fn output(
        format: &StreamFormat,
        device: &DeviceInfo,
        user: Arc<T>,
        buffers: usize,
        frames: usize,
    ) -> io::Result<Self> {
        let mut raw = null_mut();
        // SAFETY: as in input.
        status("AudioQueueNewOutput", unsafe {
            AudioQueueNewOutput(format, drained::<T>, Self::context(&user), null(), null(), 0, &mut raw)
        })?;
        let q = Self { raw, user };
        q.set_device(&device.uid, device.outputs)?;
        for _ in 0..buffers {
            let b = q.allocate(frames)?;
            // SAFETY: b was just allocated from the live queue and is not enqueued yet, so it is ours to fill.
            unsafe { refill(&*q.user, q.raw, b, 0)? };
        }
        Ok(q)
    }
}
