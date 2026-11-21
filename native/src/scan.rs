//! Reads one block of a CSV file and finds its structure: where every comma
//! and newline is, and how many quotes come before each line end. Lines are
//! then handed out as fields with their wrapping quotes already removed.
//!
//! A line belongs to the block that holds the newline before it. So block 0
//! never yields the header, and a line crossing a block edge is read once.

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;

const PAD: usize = 64;
const EXTRA: usize = 4096;
/// A separator's entry: its offset, and whether a quote follows or precedes it.
const AT: u32 = (1 << 30) - 1;
const AFTER: u32 = 30;
const BEFORE: u32 = 31;

/// How far past a block's end its last line may run before we stop trusting
/// the mapping and read the block instead.
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
    /// Where offsets count from: the block's first byte.
    base: *const u8,
    /// Newlines before this offset start lines that are ours.
    own_end: u32,
    seps: Vec<u32>,
    newlines: Vec<u32>,
    quote_mask: Vec<u64>,
    quote_sum: Vec<u32>,
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn masks(p: *const u8) -> (u64, u64, u64) {
    #[cfg(target_feature = "avx2")]
    unsafe {
        use std::arch::x86_64::*;
        let low = _mm256_loadu_si256(p as *const __m256i);
        let high = _mm256_loadu_si256(p.add(32) as *const __m256i);
        let eq = |needle: u8| -> u64 {
            let n = _mm256_set1_epi8(needle as i8);
            let a = _mm256_movemask_epi8(_mm256_cmpeq_epi8(low, n)) as u32 as u64;
            let b = _mm256_movemask_epi8(_mm256_cmpeq_epi8(high, n)) as u32 as u64;
            a | (b << 32)
        };
        (eq(b','), eq(b'\n'), eq(b'"'))
    }
    #[cfg(not(target_feature = "avx2"))]
    unsafe {
        masks_scalar(p)
    }
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn masks(p: *const u8) -> (u64, u64, u64) {
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
        let eq = |needle: u8| -> u64 {
            let n = vdupq_n_u8(needle);
            let t0 = vandq_u8(vceqq_u8(chunks[0], n), bit);
            let t1 = vandq_u8(vceqq_u8(chunks[1], n), bit);
            let t2 = vandq_u8(vceqq_u8(chunks[2], n), bit);
            let t3 = vandq_u8(vceqq_u8(chunks[3], n), bit);
            let s0 = vpaddq_u8(t0, t1);
            let s1 = vpaddq_u8(t2, t3);
            let s = vpaddq_u8(s0, s1);
            let s = vpaddq_u8(s, s);
            vgetq_lane_u64(vreinterpretq_u64_u8(s), 0)
        };
        (eq(b','), eq(b'\n'), eq(b'"'))
    }
}

#[allow(dead_code)]
#[inline(always)]
unsafe fn masks_scalar(p: *const u8) -> (u64, u64, u64) {
    unsafe {
        let (mut commas, mut newlines, mut quotes) = (0u64, 0u64, 0u64);
        for at in 0..64 {
            let byte = *p.add(at);
            commas |= u64::from(byte == b',') << at;
            newlines |= u64::from(byte == b'\n') << at;
            quotes |= u64::from(byte == b'"') << at;
        }
        (commas, newlines, quotes)
    }
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
#[inline(always)]
unsafe fn masks(p: *const u8) -> (u64, u64, u64) {
    unsafe { masks_scalar(p) }
}

impl Block {
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            base: std::ptr::null(),
            own_end: 0,
            seps: Vec::new(),
            newlines: Vec::new(),
            quote_mask: Vec::new(),
            quote_sum: Vec::new(),
        }
    }

    fn reserve(&mut self, data: usize) {
        let need = PAD + data + 1 + 2 * PAD;
        if self.buf.len() < need {
            self.buf.resize(need, 0);
        }
    }

    /// Loads the block `[start, end)` of `source`, plus the rest of the line
    /// that crosses `end`.
    pub fn load(&mut self, source: &Source, start: u64, end: u64) -> io::Result<()> {
        let own = (end - start) as usize;
        self.own_end = own as u32;
        if !source.map.is_null() && end + SLACK as u64 <= source.size {
            // Straight from the page cache: everything we touch is inside the file.
            self.base = unsafe { source.map.add(start as usize) };
            #[cfg(target_os = "linux")]
            unsafe {
                // MADV_POPULATE_READ: map the block's pages in one call, not a fault each.
                let from = self.base as usize & !4095;
                madvise(from as *mut _, self.base as usize + own + 4096 - from, 22);
            }
            if self.index(own, own + SLACK - 64) {
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
            // The last line we own may run past `end`: it needs its newline.
            let from = PAD + have.max(own);
            if at_end || self.buf[from..PAD + want].contains(&b'\n') {
                have = want;
                break;
            }
            have = want;
            extra *= 8;
        }
        if have > 0 && self.buf[PAD + have - 1] != b'\n' {
            // Only at the end of the file: the last line has no newline.
            self.buf[PAD + have] = b'\n';
            have += 1;
        }
        self.buf[PAD + have..PAD + have + 2 * PAD].fill(0);
        self.base = unsafe { self.buf.as_ptr().add(PAD) };
        self.index(have, have);
        Ok(())
    }

    /// Indexes at least the first `need` bytes, then on up to `limit` until a
    /// newline at or after `own_end` is in the index. False if none was found.
    fn index(&mut self, need: usize, limit: usize) -> bool {
        let most = limit.div_ceil(64);
        if self.seps.len() < most * 64 + 72 {
            self.seps = vec![0u32; most * 64 + 72];
        }
        if self.quote_mask.len() < most + 1 {
            self.quote_mask = vec![0u64; most + 1];
            self.quote_sum = vec![0u32; most + 1];
        }
        self.newlines.clear();
        let data = self.base;
        let seps = self.seps.as_mut_ptr();
        let own_words = need.div_ceil(64);
        let mut count = 0usize;
        let mut quotes = 0u32;
        let mut word = 0;
        let mut closed = false;
        // Each separator also records whether a quote comes right after it and
        // right before it, which needs the quotes of the words on either side.
        let mut carried = 0u64;
        let mut ahead = if most > 0 { unsafe { masks(data) } } else { (0, 0, 0) };
        while word < most {
            let (commas, newlines, quote) = ahead;
            ahead = if word + 1 < most {
                unsafe { masks(data.add((word + 1) * 64)) }
            } else {
                (0, 0, 0)
            };
            let after = (quote >> 1) | (ahead.2 << 63);
            let before = (quote << 1) | carried;
            carried = quote >> 63;
            unsafe {
                *self.quote_mask.get_unchecked_mut(word) = quote;
                *self.quote_sum.get_unchecked_mut(word) = quotes;
            }
            quotes += quote.count_ones();
            let all = commas | newlines;
            let mut rest = newlines;
            while rest != 0 {
                let bit = rest.trailing_zeros();
                let before = (all & ((1u64 << bit) - 1)).count_ones();
                self.newlines.push(count as u32 + before);
                closed |= word * 64 + bit as usize >= self.own_end as usize;
                rest &= rest - 1;
            }
            let base = (word * 64) as u32;
            let total = all.count_ones() as usize;
            let mut rest = all;
            let mut done = 0;
            while done < total {
                unsafe {
                    for slot in 0..8 {
                        let bit = rest.trailing_zeros();
                        let flags =
                            ((after.wrapping_shr(bit) & 1) << AFTER) | ((before.wrapping_shr(bit) & 1) << BEFORE);
                        *seps.add(count + done + slot) = (base + bit) | flags as u32;
                        rest &= rest.wrapping_sub(1);
                    }
                }
                done += 8;
            }
            count += total;
            word += 1;
            if word >= own_words && closed {
                break;
            }
        }
        self.quote_mask[word] = 0;
        self.quote_sum[word] = quotes;
        closed || need == limit
    }

    #[inline(always)]
    fn quotes_before(&self, at: u32) -> u32 {
        let index = at as usize >> 6;
        let bit = at as usize & 63;
        unsafe {
            *self.quote_sum.get_unchecked(index)
                + (*self.quote_mask.get_unchecked(index) & ((1u64 << bit) - 1)).count_ones()
        }
    }

    /// Calls `visit` with the block's base and the `N` fields (start, end) of
    /// every well-formed line of this block, quotes removed.
    #[inline(always)]
    pub fn lines<const N: usize>(&self, mut visit: impl FnMut(*const u8, &[(u32, u32); N])) {
        let newlines = &self.newlines[..];
        if newlines.is_empty() {
            return;
        }
        let buf = self.base;
        let seps = self.seps.as_ptr();
        unsafe {
            let mut previous = newlines[0] as usize;
            let mut previous_sep = *seps.add(previous);
            let mut previous_at = previous_sep & AT;
            let mut previous_quotes = self.quotes_before(previous_at);
            for &newline in &newlines[1..] {
                if previous_at >= self.own_end {
                    break;
                }
                let index = newline as usize;
                let sep = *seps.add(index);
                let at = sep & AT;
                let quotes = self.quotes_before(at);
                if index - previous == N {
                    let mut fields = [(0u32, 0u32); N];
                    let mut start = previous_at + 1;
                    let mut opens = (previous_sep >> AFTER) & 1;
                    let mut ok = true;
                    let mut quoted = 0u32;
                    for (field, slot) in fields.iter_mut().enumerate() {
                        let sep = *seps.add(previous + 1 + field);
                        let end = sep & AT;
                        let closes = (sep >> BEFORE) & u32::from(end - start >= 2);
                        ok &= opens <= closes;
                        quoted += opens;
                        *slot = (start + opens, end.wrapping_sub(opens));
                        start = end + 1;
                        opens = (sep >> AFTER) & 1;
                    }
                    if ok && quoted * 2 == quotes - previous_quotes {
                        visit(buf, &fields);
                    }
                }
                previous = index;
                previous_sep = sep;
                previous_at = at;
                previous_quotes = quotes;
            }
        }
    }
}
