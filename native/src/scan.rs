//! Reads one block of a CSV file and hands out its lines as fields, with their
//! wrapping quotes already removed. One pass: for each line, the commas,
//! newlines and quotes of the next 64 bytes are found at once, and the fields
//! are read off those bit masks.
//!
//! A line belongs to the block that holds the newline before it. So block 0
//! never yields the header, and a line crossing a block edge is read once.

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

/// How many bytes ahead of the scan to prefetch input. The input is mapped in
/// 4 KB pages and hardware prefetchers stop at each page's end; on the Intel
/// machines this was measured on, reading ahead by hand made the scan up to
/// twice as fast, and it cost nothing where it did not help.
const READ_AHEAD: usize = 1024;

#[inline(always)]
fn fetch(target: *const u8) {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        use std::arch::x86_64::{_MM_HINT_T0, _mm_prefetch};
        _mm_prefetch(target as *const i8, _MM_HINT_T0);
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = target;
}

pub static POPULATE_FAILED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
pub static POPULATE_ERRNO: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn __errno_location() -> *mut i32;
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

#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn window(p: *const u8) -> (u64, u64) {
    unsafe {
        use std::arch::aarch64::*;
        let bits: [u8; 16] = [1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128];
        let bit = vld1q_u8(bits.as_ptr());
        let chunks = [
            vld1q_u8(p),
            vld1q_u8(p.add(16)),
            vld1q_u8(p.add(32)),
            vld1q_u8(p.add(48)),
        ];
        let (comma, newline, quote) = (vdupq_n_u8(b','), vdupq_n_u8(b'\n'), vdupq_n_u8(b'"'));
        let pack = |m: [uint8x16_t; 4]| -> u64 {
            let s0 = vpaddq_u8(vandq_u8(m[0], bit), vandq_u8(m[1], bit));
            let s1 = vpaddq_u8(vandq_u8(m[2], bit), vandq_u8(m[3], bit));
            let s = vpaddq_u8(s0, s1);
            let s = vpaddq_u8(s, s);
            vgetq_lane_u64(vreinterpretq_u64_u8(s), 0)
        };
        let ends = |v: uint8x16_t| vorrq_u8(vceqq_u8(v, comma), vceqq_u8(v, newline));
        (
            pack([ends(chunks[0]), ends(chunks[1]), ends(chunks[2]), ends(chunks[3])]),
            pack([
                vceqq_u8(chunks[0], quote),
                vceqq_u8(chunks[1], quote),
                vceqq_u8(chunks[2], quote),
                vceqq_u8(chunks[3], quote),
            ]),
        )
    }
}

#[cfg(not(any(all(target_arch = "x86_64", target_feature = "avx2"), target_arch = "aarch64")))]
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
                if madvise(from as *mut _, self.base as usize + own + 4096 - from, 22) != 0 {
                    POPULATE_FAILED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    POPULATE_ERRNO.store(*__errno_location() as usize, std::sync::atomic::Ordering::Relaxed);
                }
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

    /// Benchmark: the newlines and commas of the block, and nothing else.
    pub fn count(&self) -> u64 {
        let mut total = 0u64;
        let mut at = 0;
        while at < self.end {
            total += unsafe { window(self.base.add(at)) }.0.count_ones() as u64;
            at += 64;
        }
        total
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
            let limit = (end + 63) & !63;
            let mut index = Index::new(newline + 1);
            // `at..have` are the separators found and not yet used.
            let (mut at, mut have) = (0usize, 0usize);
            let mut quotes_so_far = 0u32;
            loop {
                while at + N <= have {
                    if newline >= own_end || newline + 1 >= end {
                        return;
                    }
                    let start = newline + 1;
                    let last = *index.separators.get_unchecked(at + N - 1) as usize;
                    let mut shaped = *buf.add(last) == b'\n';
                    for field in 0..N - 1 {
                        shaped &= *buf.add(*index.separators.get_unchecked(at + field) as usize) == b',';
                    }
                    let mut fields = [(0u32, 0u32); N];
                    let ok;
                    if shaped && index.done - start < (RING - 1) * 64 {
                        let mut fine = true;
                        let mut opened = 0u32;
                        let mut from = start;
                        for (field, slot) in fields.iter_mut().enumerate() {
                            let stop = *index.separators.get_unchecked(at + field) as usize;
                            let opens = u32::from(*buf.add(from) == b'"');
                            let closes = u32::from(*buf.add(stop - 1) == b'"');
                            fine &= opens == closes && (opens == 0 || stop - from >= 2);
                            opened += opens;
                            *slot = (from as u32 + opens, stop as u32 - opens);
                            from = stop + 1;
                        }
                        let quotes = index.quotes_before(last);
                        ok = fine && quotes.wrapping_sub(quotes_so_far) == 2 * opened;
                        quotes_so_far = quotes;
                        at += N;
                        newline = last;
                    } else {
                        let (fine, stop) = odd_line(buf, start, &mut fields);
                        ok = fine;
                        newline = stop;
                        // Find our place in the separators again, past that line.
                        if stop >= index.done {
                            while stop >= index.done && index.done < limit {
                                have = index.more(buf, have, have, limit);
                                at = 0;
                            }
                        }
                        while at < have && *index.separators.get_unchecked(at) as usize <= stop {
                            at += 1;
                        }
                        quotes_so_far = index.quotes_before(stop);
                    }
                    if ok {
                        visit(buf, &fields);
                    }
                }
                if index.done >= limit {
                    return;
                }
                have = index.more(buf, at, have, limit);
                at = 0;
            }
        }
    }
}

/// 64-byte windows indexed at a time, and how many windows of quote counts are kept.
const STRIP: usize = 64;
const RING: usize = 256;

/// The separators of the next few kilobytes, found ahead of the lines that use them.
struct Index {
    separators: [u32; STRIP * 64 + 128],
    quote_bits: [u64; RING],
    quotes_until: [u32; RING],
    quotes: u32,
    /// Bytes below this are indexed; a multiple of 64.
    done: usize,
    /// Clears the bits before the first line, in the first window only.
    keep: u64,
}

impl Index {
    #[inline(always)]
    fn new(start: usize) -> Self {
        Self {
            separators: [0; STRIP * 64 + 128],
            quote_bits: [0; RING],
            quotes_until: [0; RING],
            quotes: 0,
            done: start & !63,
            keep: !0u64 << (start & 63),
        }
    }

    /// Quotes before byte `at`, which must be in the last `RING` windows.
    #[inline(always)]
    unsafe fn quotes_before(&self, at: usize) -> u32 {
        unsafe {
            let word = (at >> 6) & (RING - 1);
            let below = *self.quote_bits.get_unchecked(word) & ((1u64 << (at & 63)) - 1);
            self.quotes_until.get_unchecked(word).wrapping_add(below.count_ones())
        }
    }

    /// Drops the used separators `..at`, indexes one more strip, and returns the new count.
    #[inline(always)]
    unsafe fn more(&mut self, buf: *const u8, at: usize, have: usize, limit: usize) -> usize {
        unsafe {
            let mut have = have - at;
            if at > 0 && have > 0 {
                self.separators.copy_within(at..at + have, 0);
            }
            let stop = (self.done + STRIP * 64).min(limit);
            let out = self.separators.as_mut_ptr();
            while self.done < stop {
                fetch(buf.wrapping_add(self.done + READ_AHEAD));
                let (ends, quotes) = window(buf.add(self.done));
                let (mut ends, quotes) = (ends & self.keep, quotes & self.keep);
                self.keep = !0;
                let word = (self.done >> 6) & (RING - 1);
                *self.quote_bits.get_unchecked_mut(word) = quotes;
                *self.quotes_until.get_unchecked_mut(word) = self.quotes;
                self.quotes = self.quotes.wrapping_add(quotes.count_ones());
                let count = ends.count_ones() as usize;
                let base = self.done as u32;
                let mut to = out.add(have);
                for _ in 0..8 {
                    *to = base + ends.trailing_zeros();
                    ends &= ends.wrapping_sub(1);
                    to = to.add(1);
                }
                if count > 8 {
                    for _ in 0..8 {
                        *to = base + ends.trailing_zeros();
                        ends &= ends.wrapping_sub(1);
                        to = to.add(1);
                    }
                    while ends != 0 {
                        *to = base + ends.trailing_zeros();
                        ends &= ends.wrapping_sub(1);
                        to = to.add(1);
                    }
                }
                have += count;
                self.done += 64;
            }
            have
        }
    }
}

/// One line the fast path would not take: its fields, whether it is a good
/// line of `N` fields, and where its newline is.
#[cold]
#[inline(never)]
unsafe fn odd_line<const N: usize>(buf: *const u8, first: usize, fields: &mut [(u32, u32); N]) -> (bool, usize) {
    unsafe {
        let mut count = 0usize;
        let mut ok = true;
        let mut start = first;
        loop {
            let (ends, quotes) = window(buf.add(start));
            let (stop, fine, opens) = if ends != 0 {
                let length = ends.trailing_zeros();
                let inside = quotes & ((1u64 << length) - 1);
                let opens = inside & 1;
                let wanted = opens * (1 | 1u64.wrapping_shl(length.wrapping_sub(1)));
                (
                    start + length as usize,
                    inside == wanted && (opens == 0 || length >= 2),
                    opens as usize,
                )
            } else {
                long_field(buf, start)
            };
            ok &= fine;
            if count < N {
                fields[count] = ((start + opens) as u32, (stop - opens) as u32);
            }
            count += 1;
            if *buf.add(stop) == b'\n' {
                return (ok && count == N, stop);
            }
            start = stop + 1;
        }
    }
}
