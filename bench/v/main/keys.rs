//! IDs as 64-bit keys. An ID of 1 to 9 bytes, each 1..=127, packs into 63 bits
//! with its first byte most significant, so packed keys order numerically the
//! way their strings order byte-wise. Anything else (longer, non-ASCII, a NUL)
//! is interned behind a lock and gets bit 63. Key 0 is the empty string.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Mutex;

pub const INTERNED: u64 = 1 << 63;

struct Unusual {
    ids: HashMap<Vec<u8>, u64>,
    names: Vec<Vec<u8>>,
}

static UNUSUAL: Mutex<Option<Unusual>> = Mutex::new(None);

#[cold]
#[inline(never)]
fn intern(value: &[u8]) -> u64 {
    let mut guard = UNUSUAL.lock().unwrap();
    let table = guard.get_or_insert_with(|| Unusual {
        ids: HashMap::new(),
        names: Vec::new(),
    });
    if let Some(&key) = table.ids.get(value) {
        return key;
    }
    let key = INTERNED | table.names.len() as u64;
    table.ids.insert(value.to_vec(), key);
    table.names.push(value.to_vec());
    key
}

/// The key of the `len` bytes at `p`. At least 9 bytes must be readable at `p`.
#[cfg(all(target_arch = "x86_64", target_feature = "bmi2"))]
#[inline(always)]
pub unsafe fn pack(p: *const u8, len: usize) -> u64 {
    unsafe {
        use std::arch::x86_64::{_bzhi_u64, _pext_u64};
        const LOW: u64 = 0x7F7F_7F7F_7F7F_7F7F;
        const HIGH: u64 = 0x8080_8080_8080_8080;
        if len == 0 {
            return 0;
        }
        if len <= 9 {
            // The first eight bytes, with those past the end cleared.
            let x = _bzhi_u64(u64::from_le((p as *const u64).read_unaligned()), len as u32 * 8);
            // 0x80 in every byte that is 1..=127: wanted in each byte of the ID.
            let plain = (x & LOW).wrapping_add(LOW) & !x & HIGH;
            let ninth = if len == 9 { *p.add(8) } else { 1 };
            if plain == _bzhi_u64(HIGH, len as u32 * 8) && ninth.wrapping_sub(1) < 127 {
                let ninth = if len == 9 { ninth as u64 } else { 0 };
                return (_pext_u64(x.swap_bytes(), LOW) << 7) | ninth;
            }
        }
        intern(std::slice::from_raw_parts(p, len))
    }
}

/// The key of the `len` bytes at `p`. At least 9 bytes must be readable at `p`.
#[cfg(not(all(target_arch = "x86_64", target_feature = "bmi2")))]
#[inline(always)]
pub unsafe fn pack(p: *const u8, len: usize) -> u64 {
    unsafe { pack_plain(p, len) }
}

/// `pack` without any special instructions.
#[inline(always)]
pub unsafe fn pack_plain(p: *const u8, len: usize) -> u64 {
    unsafe {
        if len == 0 {
            return 0;
        }
        if len <= 9 {
            let raw = u64::from_le((p as *const u64).read_unaligned());
            let mask = if len >= 8 { !0u64 } else { (1u64 << (len * 8)) - 1 };
            let x = raw & mask;
            let filled = x | !mask;
            let zero = filled.wrapping_sub(0x0101_0101_0101_0101) & !filled & 0x8080_8080_8080_8080;
            let ninth = if len == 9 { *p.add(8) } else { 0 };
            let ninth_ok = len < 9 || (ninth != 0 && ninth < 128);
            if (x & 0x8080_8080_8080_8080) | zero == 0 && ninth_ok {
                let mut y = x.swap_bytes();
                y = ((y & 0x7F00_7F00_7F00_7F00) >> 1) | (y & 0x007F_007F_007F_007F);
                y = ((y & 0x3FFF_0000_3FFF_0000) >> 2) | (y & 0x0000_3FFF_0000_3FFF);
                y = ((y & 0x0FFF_FFFF_0000_0000) >> 4) | (y & 0x0000_0000_0FFF_FFFF);
                return (y << 7) | ninth as u64;
            }
        }
        intern(std::slice::from_raw_parts(p, len))
    }
}

/// Appends the string of `key` to `out`.
#[inline]
pub fn unpack(key: u64, out: &mut Vec<u8>) {
    if key & INTERNED == 0 {
        let mut shift = 56i32;
        while shift >= 0 {
            let byte = ((key >> shift) & 0x7F) as u8;
            if byte == 0 {
                break;
            }
            out.push(byte);
            shift -= 7;
        }
    } else {
        let guard = UNUSUAL.lock().unwrap();
        out.extend_from_slice(&guard.as_ref().unwrap().names[(key & !INTERNED) as usize]);
    }
}

#[cold]
#[inline(never)]
fn compare_slow(a: u64, b: u64) -> Ordering {
    let (mut x, mut y) = (Vec::new(), Vec::new());
    unpack(a, &mut x);
    unpack(b, &mut y);
    x.cmp(&y)
}

/// Byte-wise order of the two keys' strings.
#[inline(always)]
pub fn compare(a: u64, b: u64) -> Ordering {
    if (a | b) & INTERNED == 0 {
        a.cmp(&b)
    } else {
        compare_slow(a, b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_matches_plain() {
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for round in 0..2_000_000 {
            let mut bytes = [0u8; 24];
            for byte in bytes.iter_mut() {
                let random = next();
                // Mostly ID characters, sometimes a NUL or a byte above 127.
                *byte = match random % 50 {
                    0 => 0,
                    1 => 128 + (random >> 8) as u8 % 128,
                    2 => 1 + (random >> 8) as u8 % 127,
                    _ => b"abcdefghijklmnopqrstuvwxyz0123456789_"[(random >> 8) as usize % 37],
                };
            }
            let len = round % 12;
            let (fast, plain) = unsafe { (pack(bytes.as_ptr(), len), pack_plain(bytes.as_ptr(), len)) };
            if fast & INTERNED == 0 || plain & INTERNED == 0 {
                assert_eq!(fast, plain, "{:?} {len}", &bytes[..len]);
            }
        }
    }
}
