//! The alternative scanner (cargo feature `fused`): one pass, no index. For
//! each line the commas, newlines and quotes of the next 64 bytes are found at
//! once and the fields are read off those bit masks.

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;

const PAD: usize = 64;
const EXTRA: usize = 4096;
const SLACK: usize = 1 << 16;

unsafe extern "C" {
    fn mmap(
        address: *mut std::ffi::c_void,
        length: usize,
        protection: i32,
        flags: i32,
        fd: i32,
        offset: i64,
    ) -> *mut std::ffi::c_void;
    #[cfg(target_os = "linux")]
    fn madvise(address: *mut std::ffi::c_void, length: usize, advice: i32) -> i32;
}

/// An input file, mapped when it can be.
pub struct Source {
    file: File,
    pub size: u64,
    map: *const u8,
}

unsafe impl Send for Source {}
unsafe impl Sync for Source {}

impl Source {
    pub fn open(path: &std::path::Path, map: bool) -> io::Result<Self> {
        use std::os::fd::AsRawFd;
        let file = File::open(path)?;
        let size = file.metadata()?.len();
        let mut mapped = std::ptr::null();
        if map && size > 0 {
            let address = unsafe { mmap(std::ptr::null_mut(), size as usize, 1, 2, file.as_raw_fd(), 0) };
            if address as usize != usize::MAX {
                mapped = address as *const u8;
            }
        }
        Ok(Self {
            file,
            size,
            map: mapped,
        })
    }

    /// Unmaps the pages of `[start, end)` now, from whichever thread calls. Left
    /// mapped, the kernel would unmap the whole file on one thread at exit,
    /// inside the timed run.
    pub fn release(&self, start: u64, end: u64) {
        #[cfg(target_os = "linux")]
        if !self.map.is_null() {
            let from = (self.map as usize + start as usize + 4095) & !4095;
            let to = (self.map as usize + end as usize) & !4095;
            if to > from {
                unsafe { madvise(from as *mut _, to - from, 4) };
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = (start, end);
    }
}

pub struct Block {
    buf: Vec<u8>,
    base: *const u8,
    own_end: usize,
    /// One past the newline that ends our last line; 128 bytes after it are readable.
    end: usize,
}

#[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
#[inline(always)]
unsafe fn window(p: *const u8) -> (u64, u64) {
    unsafe {
        use std::arch::x86_64::*;
        let low = _mm256_loadu_si256(p as *const __m256i);
        let high = _mm256_loadu_si256(p.add(32) as *const __m256i);
        let (comma, newline, quote) = (
            _mm256_set1_epi8(b',' as i8),
            _mm256_set1_epi8(b'\n' as i8),
            _mm256_set1_epi8(b'"' as i8),
        );
        let ends = |v: __m256i| _mm256_or_si256(_mm256_cmpeq_epi8(v, comma), _mm256_cmpeq_epi8(v, newline));
        let bits = |v: __m256i| _mm256_movemask_epi8(v) as u32 as u64;
        (
            bits(ends(low)) | (bits(ends(high)) << 32),
            bits(_mm256_cmpeq_epi8(low, quote)) | (bits(_mm256_cmpeq_epi8(high, quote)) << 32),
        )
    }
}

#[cfg(not(all(target_arch = "x86_64", target_feature = "avx2")))]
#[inline(always)]
unsafe fn window(p: *const u8) -> (u64, u64) {
    unsafe {
        let (mut ends, mut quotes) = (0u64, 0u64);
        for at in 0..64 {
            let byte = *p.add(at);
            ends |= u64::from(byte == b',' || byte == b'\n') << at;
            quotes |= u64::from(byte == b'"') << at;
        }
        (ends, quotes)
    }
}

#[cold]
#[inline(never)]
unsafe fn long_field(buf: *const u8, start: usize) -> (usize, bool, usize) {
    unsafe {
        let mut end = start + 64;
        loop {
            let (ends, _) = window(buf.add(end));
            if ends != 0 {
                end += ends.trailing_zeros() as usize;
                break;
            }
            end += 64;
        }
        let bytes = std::slice::from_raw_parts(buf.add(start), end - start);
        let quotes = bytes.iter().filter(|&&byte| byte == b'"').count();
        let opens = usize::from(bytes[0] == b'"');
        let ok = if opens == 1 {
            quotes == 2 && bytes[bytes.len() - 1] == b'"'
        } else {
            quotes == 0
        };
        (end, ok, opens)
    }
}

impl Block {
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            base: std::ptr::null(),
            own_end: 0,
            end: 0,
        }
    }

    fn reserve(&mut self, data: usize) {
        let need = PAD + data + 1 + 4 * PAD;
        if self.buf.len() < need {
            self.buf.resize(need, 0);
        }
    }

    pub fn load(&mut self, source: &Source, start: u64, end: u64) -> io::Result<()> {
        let own = (end - start) as usize;
        self.own_end = own;
        if !source.map.is_null() && end + SLACK as u64 <= source.size {
            self.base = unsafe { source.map.add(start as usize) };
            #[cfg(target_os = "linux")]
            unsafe {
                let from = self.base as usize & !4095;
                madvise(from as *mut _, self.base as usize + own + 4096 - from, 22);
            }
            let tail = unsafe { std::slice::from_raw_parts(self.base.add(own), SLACK - 4 * PAD) };
            if let Some(at) = tail.iter().position(|&byte| byte == b'\n') {
                self.end = own + at + 1;
                return Ok(());
            }
        }
        let size = source.size;
        let mut have = 0usize;
        let mut extra = EXTRA;
        loop {
            let want = (own + extra).min((size - start) as usize);
            self.reserve(want);
            source
                .file
                .read_exact_at(&mut self.buf[PAD + have..PAD + want], start + have as u64)?;
            let at_end = start + want as u64 == size;
            let from = PAD + have.max(own);
            let found = self.buf[from..PAD + want].iter().position(|&byte| byte == b'\n');
            have = want;
            if let Some(at) = found {
                have = from - PAD + at + 1;
                break;
            }
            if at_end {
                break;
            }
            extra *= 8;
        }
        if have > 0 && self.buf[PAD + have - 1] != b'\n' {
            self.buf[PAD + have] = b'\n';
            have += 1;
        }
        self.buf[PAD + have..PAD + have + 4 * PAD].fill(0);
        self.base = unsafe { self.buf.as_ptr().add(PAD) };
        self.end = have;
        Ok(())
    }

    #[inline(always)]
    pub fn lines<const N: usize>(&self, mut visit: impl FnMut(*const u8, &[(u32, u32); N])) {
        let buf = self.base;
        let (own_end, end) = (self.own_end, self.end);
        unsafe {
            let head = std::slice::from_raw_parts(buf, own_end.min(end));
            let Some(mut newline) = head.iter().position(|&byte| byte == b'\n') else {
                return;
            };
            while newline < own_end && newline + 1 < end {
                let mut fields = [(0u32, 0u32); N];
                let mut count = 0usize;
                let mut ok = true;
                let mut start = newline + 1;
                let mut at = start;
                let (mut ends, mut quotes) = window(buf.add(at));
                loop {
                    let mut shift = start - at;
                    let mut rest = if shift < 64 { ends >> shift } else { 0 };
                    if rest == 0 {
                        at = start;
                        (ends, quotes) = window(buf.add(at));
                        shift = 0;
                        rest = ends;
                    }
                    let (stop, fine, opens) = if rest != 0 {
                        let length = rest.trailing_zeros();
                        let inside = (quotes >> shift) & ((1u64 << length) - 1);
                        let opens = inside & 1;
                        let wanted = opens * (1 | 1u64.wrapping_shl(length.wrapping_sub(1)));
                        (
                            start + length as usize,
                            inside == wanted && (opens == 0 || length >= 2),
                            opens as usize,
                        )
                    } else {
                        ends = 0;
                        long_field(buf, start)
                    };
                    ok &= fine;
                    if count < N {
                        fields[count] = ((start + opens) as u32, (stop - opens) as u32);
                    }
                    count += 1;
                    if *buf.add(stop) == b'\n' {
                        newline = stop;
                        break;
                    }
                    start = stop + 1;
                }
                if ok && count == N {
                    visit(buf, &fields);
                }
            }
        }
    }
}
