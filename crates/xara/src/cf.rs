use std::ffi::{CString, c_char, c_void};
use std::io;
use std::ptr::{NonNull, null};

pub type CfIndex = isize;

const UTF8: u32 = 0x0800_0100;
const NUMBER_SINT32: CfIndex = 3;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeDictionaryKeyCallBacks: u8;
    static kCFTypeDictionaryValueCallBacks: u8;
    fn CFRetain(cf: *const c_void) -> *const c_void;
    fn CFRelease(cf: *const c_void);
    fn CFGetTypeID(cf: *const c_void) -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFStringCreateWithCString(alloc: *const c_void, s: *const c_char, enc: u32) -> *const c_void;
    fn CFStringGetLength(s: *const c_void) -> CfIndex;
    fn CFStringGetMaximumSizeForEncoding(len: CfIndex, enc: u32) -> CfIndex;
    fn CFStringGetCString(s: *const c_void, buf: *mut c_char, size: CfIndex, enc: u32) -> u8;
    fn CFNumberCreate(alloc: *const c_void, kind: CfIndex, v: *const c_void) -> *const c_void;
    fn CFDictionaryCreate(
        alloc: *const c_void,
        keys: *const *const c_void,
        values: *const *const c_void,
        n: CfIndex,
        key_cb: *const c_void,
        value_cb: *const c_void,
    ) -> *const c_void;
    fn CFSetGetCount(s: *const c_void) -> CfIndex;
    fn CFSetGetValues(s: *const c_void, values: *mut *const c_void);
}

pub struct Cf(NonNull<c_void>);

impl Drop for Cf {
    fn drop(&mut self) {
        // SAFETY: self holds one reference to a live CF object and gives it up here, once.
        unsafe { CFRelease(self.ptr()) }
    }
}

impl Cf {
    pub unsafe fn owned(p: *const c_void) -> Option<Self> {
        NonNull::new(p.cast_mut()).map(Self)
    }

    pub unsafe fn created(p: *const c_void, what: &str) -> io::Result<Self> {
        // SAFETY: the caller hands over one reference to p, or null.
        unsafe { Self::owned(p) }.ok_or_else(|| io::Error::other(format!("{what} failed")))
    }

    pub unsafe fn retained(p: *const c_void) -> io::Result<Self> {
        // SAFETY: the caller vouches that p is a live CF object, and CFRetain takes a reference of our own.
        unsafe { Self::created(CFRetain(p), "CFRetain") }
    }

    pub fn ptr(&self) -> *const c_void {
        self.0.as_ptr()
    }

    pub fn string(s: &str) -> io::Result<Self> {
        let c = CString::new(s).map_err(|_| io::Error::other(format!("{s:?}: contains a NUL byte")))?;
        // SAFETY: c is NUL-terminated UTF-8 that outlives the call, and Create hands over one reference for Cf.
        unsafe { Self::owned(CFStringCreateWithCString(null(), c.as_ptr(), UTF8)) }
            .ok_or_else(|| io::Error::other("CFStringCreateWithCString failed"))
    }

    pub fn number(v: i32) -> io::Result<Self> {
        // SAFETY: v outlives the call and is the SInt32 that NUMBER_SINT32 says it is; Create hands over one reference.
        unsafe { Self::created(CFNumberCreate(null(), NUMBER_SINT32, (&raw const v).cast()), "CFNumberCreate") }
    }

    pub fn dictionary(pairs: &[(&Cf, &Cf)]) -> io::Result<Self> {
        let keys: Vec<_> = pairs.iter().map(|(k, _)| k.ptr()).collect();
        let values: Vec<_> = pairs.iter().map(|(_, v)| v.ptr()).collect();
        // SAFETY: keys and values hold live CF objects, the callbacks retain them, and Create hands over one reference.
        unsafe {
            let d = CFDictionaryCreate(
                null(),
                keys.as_ptr(),
                values.as_ptr(),
                keys.len() as CfIndex,
                (&raw const kCFTypeDictionaryKeyCallBacks).cast(),
                (&raw const kCFTypeDictionaryValueCallBacks).cast(),
            );
            Self::created(d, "CFDictionaryCreate")
        }
    }

    pub unsafe fn set_values(&self) -> Vec<*const c_void> {
        // SAFETY: the caller vouches that self is a CFSet, and v has room for all of its values.
        unsafe {
            let mut v = vec![null(); usize::try_from(CFSetGetCount(self.ptr())).unwrap_or(0)];
            CFSetGetValues(self.ptr(), v.as_mut_ptr());
            v
        }
    }

    pub fn utf8(&self) -> Option<String> {
        // SAFETY: self is a live CF object and both calls take any CF type.
        if unsafe { CFGetTypeID(self.ptr()) != CFStringGetTypeID() } {
            return None;
        }
        // SAFETY: self is a live CFString and buf holds size bytes.
        unsafe {
            let size = CFStringGetMaximumSizeForEncoding(CFStringGetLength(self.ptr()), UTF8) + 1;
            let mut buf = vec![0u8; usize::try_from(size).ok()?];
            if CFStringGetCString(self.ptr(), buf.as_mut_ptr().cast(), size, UTF8) == 0 {
                return None;
            }
            buf.truncate(buf.iter().position(|&b| b == 0)?);
            String::from_utf8(buf).ok()
        }
    }
}
