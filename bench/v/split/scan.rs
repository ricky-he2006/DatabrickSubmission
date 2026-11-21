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

pub struct Block {
    buf: Vec<u8>,
    base: *const u8,
    own_end: usize,
    /// One past the newline that ends our last line; 128 bytes after it are readable.
    end: usize,
}

/// Masks of the 64 bytes at `p`: commas, newlines, quotes.
#[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
#[inline(always)]
unsafe fn window(p: *const u8) -> (u64, u64, u64) {
    unsafe {
        use std::arch::x86_64::*;
        let low = _mm256_loadu_si256(p as *const __m256i);
        let high = _mm256_loadu_si256(p.add(32) as *const __m256i);
        let bits = |byte: u8| {
            let wanted = _mm256_set1_epi8(byte as i8);
            let low = _mm256_movemask_epi8(_mm256_cmpeq_epi8(low, wanted)) as u32 as u64;
            let high = _mm256_movemask_epi8(_mm256_cmpeq_epi8(high, wanted)) as u32 as u64;
            low | (high << 32)
        };
        (bits(b','), bits(b'\n'), bits(b'"'))
    }
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn window(p: *const u8) -> (u64, u64, u64) {
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
        let pack = |byte: u8| -> u64 {
            let wanted = vdupq_n_u8(byte);
            let s0 = vpaddq_u8(
                vandq_u8(vceqq_u8(chunks[0], wanted), bit),
                vandq_u8(vceqq_u8(chunks[1], wanted), bit),
            );
            let s1 = vpaddq_u8(
                vandq_u8(vceqq_u8(chunks[2], wanted), bit),
                vandq_u8(vceqq_u8(chunks[3], wanted), bit),
            );
            let s = vpaddq_u8(s0, s1);
            let s = vpaddq_u8(s, s);
            vgetq_lane_u64(vreinterpretq_u64_u8(s), 0)
        };
        (pack(b','), pack(b'\n'), pack(b'"'))
    }
}

#[cfg(not(any(all(target_arch = "x86_64", target_feature = "avx2"), target_arch = "aarch64")))]
#[inline(always)]
unsafe fn window(p: *const u8) -> (u64, u64, u64) {
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

/// Strips the wrapping quotes of a line's fields in place. `quotes` is how many
/// the line holds; false when any of them is not one of a wrapping pair.
#[inline(always)]
unsafe fn unquote(buf: *const u8, fields: &mut [(u32, u32)], quotes: u32) -> bool {
    unsafe {
        let mut wrapping = 0u32;
        let mut ok = true;
        for field in fields.iter_mut() {
            let (start, stop) = *field;
            if start < stop && *buf.add(start as usize) == b'"' {
                ok &= stop - start >= 2 && *buf.add(stop as usize - 1) == b'"';
                *field = (start + 1, stop - 1);
                wrapping += 2;
            }
        }
        ok && wrapping == quotes
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

    /// Calls `visit` with the fields of every well-formed line of `N` fields.
    /// The separators of each 64 bytes are found at once and walked bit by bit;
    /// a line's quotes are counted from the same masks, and only a line that
    /// has some is checked field by field.
    #[inline(always)]
    pub fn lines<const N: usize>(&self, mut visit: impl FnMut(*const u8, &[(u32, u32); N])) {
        const { assert!(N < 16) };
        let buf = self.base;
        let (own_end, end) = (self.own_end, self.end);
        unsafe {
            let head = std::slice::from_raw_parts(buf, own_end.min(end));
            let Some(newline) = head.iter().position(|&byte| byte == b'\n') else {
                return;
            };
            if newline >= own_end || newline + 1 >= end {
                return;
            }
            let mut fields = [(0u32, 0u32); 16];
            let mut count = 0usize;
            let mut start = newline + 1;
            let mut base = start;
            // Quotes from the first line's start to `base`, and to the current line's start.
            let mut seen = 0u32;
            let mut line_quotes = 0u32;
            loop {
                fetch(buf.wrapping_add(base + READ_AHEAD));
                let (commas, newlines, quotes) = window(buf.add(base));
                let mut separators = commas | newlines;
                while separators != 0 {
                    let bit = separators.trailing_zeros();
                    separators &= separators - 1;
                    let stop = base + bit as usize;
                    *fields.get_unchecked_mut(count & 15) = (start as u32, stop as u32);
                    count += 1;
                    start = stop + 1;
                    if (newlines >> bit) & 1 != 0 {
                        let before = seen + (quotes & ((1u64 << bit) - 1)).count_ones();
                        let held = before - line_quotes;
                        line_quotes = before;
                        if count == N && (held == 0 || unquote(buf, &mut fields[..N], held)) {
                            visit(buf, &*(fields.as_ptr() as *const [(u32, u32); N]));
                        }
                        count = 0;
                        if stop >= own_end || stop + 1 >= end {
                            return;
                        }
                    }
                }
                seen += quotes.count_ones();
                base += 64;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rules, read literally: every complete line after the first newline.
    fn reference<const N: usize>(text: &[u8]) -> Vec<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        let Some(first) = text.iter().position(|&byte| byte == b'\n') else {
            return out;
        };
        let mut rest = &text[first + 1..];
        while let Some(at) = rest.iter().position(|&byte| byte == b'\n') {
            let line = &rest[..at];
            rest = &rest[at + 1..];
            let fields: Vec<&[u8]> = line.split(|&byte| byte == b',').collect();
            if fields.len() != N {
                continue;
            }
            let mut values = Vec::new();
            let mut ok = true;
            for field in fields {
                let quotes = field.iter().filter(|&&byte| byte == b'"').count();
                if quotes == 0 {
                    values.push(field.to_vec());
                } else if quotes == 2 && field.len() >= 2 && field[0] == b'"' && field[field.len() - 1] == b'"' {
                    values.push(field[1..field.len() - 1].to_vec());
                } else {
                    ok = false;
                }
            }
            if ok {
                out.push(values);
            }
        }
        out
    }

    fn split<const N: usize>(text: &[u8]) -> Vec<Vec<Vec<u8>>> {
        let mut buf = vec![0u8; PAD + text.len() + 4 * PAD];
        buf[PAD..PAD + text.len()].copy_from_slice(text);
        let block = Block {
            base: unsafe { buf.as_ptr().add(PAD) },
            own_end: text.len(),
            end: text.len(),
            buf,
        };
        let mut out = Vec::new();
        block.lines::<N>(|base, fields| {
            out.push(
                fields
                    .iter()
                    .map(|&(start, stop)| unsafe {
                        std::slice::from_raw_parts(base.add(start as usize), (stop - start) as usize).to_vec()
                    })
                    .collect(),
            );
        });
        out
    }

    #[test]
    fn matches_reference() {
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for round in 0..20_000 {
            let mut text = Vec::new();
            let lines = next() % 40;
            for _ in 0..lines {
                let fields = if next() % 8 == 0 { next() % 6 } else { 3 };
                for field in 0..fields {
                    if field > 0 {
                        text.push(b',');
                    }
                    let length = match next() % 10 {
                        0 => 0,
                        1 => 60 + next() % 150,
                        _ => next() % 12,
                    };
                    let quoted = next() % 4 == 0;
                    if quoted {
                        text.push(b'"');
                    }
                    for _ in 0..length {
                        text.push(match next() % 60 {
                            0 => b'"',
                            1 => b'\r',
                            _ => b'a' + (next() % 26) as u8,
                        });
                    }
                    if quoted && next() % 10 != 0 {
                        text.push(b'"');
                    }
                }
                text.push(b'\n');
            }
            if round % 3 == 0 && !text.is_empty() {
                text.pop();
                text.push(b'\n');
            }
            assert_eq!(split::<3>(&text), reference::<3>(&text), "{:?}", String::from_utf8_lossy(&text));
        }
    }
}
