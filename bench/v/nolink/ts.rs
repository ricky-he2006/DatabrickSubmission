//! Timestamps and prices. Every pointer passed here must have at least 32
//! readable bytes after it (the block buffers are padded).

pub const BAD: i64 = i64::MIN;

const HIGH: u64 = 0x8080_8080_8080_8080;

#[inline(always)]
unsafe fn load64(p: *const u8) -> u64 {
    unsafe { u64::from_le((p as *const u64).read_unaligned()) }
}

#[inline(always)]
unsafe fn load32(p: *const u8) -> u32 {
    unsafe { u32::from_le((p as *const u32).read_unaligned()) }
}

#[inline(always)]
fn byte(x: u64, at: u32) -> i64 {
    ((x >> (at * 8)) & 0xFF) as i64
}

#[inline(always)]
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    era * 146_097 + year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year - 719_468
}

const DAYS_IN_MONTH: [i64; 13] = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

/// The epoch day of each `YYYY-MM-DD` seen, by its day of month: a dataset
/// holds a few weeks, so nearly every date is one compare away.
pub struct Dates {
    month: [u64; 100],
    days: [i64; 100],
}

impl Dates {
    pub fn new() -> Self {
        Self {
            month: [0; 100],
            days: [0; 100],
        }
    }
}

/// The epoch day of `YYYY-MM-` (raw in `raw`) and `day`, or `BAD`.
#[cold]
#[inline(never)]
fn civil_day(raw: u64, day: i64) -> i64 {
    // XOR with the template leaves 0..=9 in digit bytes and 0 in separators.
    let a = raw ^ 0x2D30_302D_3030_3030;
    if (a.wrapping_add(0x7F76_767F_7676_7676) | a) & HIGH != 0 {
        return BAD;
    }
    let year = byte(a, 0) * 1000 + byte(a, 1) * 100 + byte(a, 2) * 10 + byte(a, 3);
    let month = byte(a, 5) * 10 + byte(a, 6);
    if !(1..=12).contains(&month) || day < 1 {
        return BAD;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    if day > DAYS_IN_MONTH[month as usize] + i64::from(leap && month == 2) {
        return BAD;
    }
    days_from_civil(year, month, day)
}

/// `YYYY-MM-DDTHH:MM:SS` at `p`, as epoch seconds; `BAD` if it is not exactly that.
#[cfg_attr(all(target_arch = "x86_64", target_feature = "avx2"), allow(dead_code))]
#[inline(always)]
unsafe fn civil_seconds_plain(p: *const u8, dates: &mut Dates) -> i64 {
    unsafe {
        let raw = load64(p); // YYYY-MM-
        let b = load64(p.add(8)) ^ 0x3030_3A30_3054_3030; // DDTHH:MM
        let c = (load32(p.add(16)) & 0x00FF_FFFF) ^ 0x0030_303A; // :SS
        let bad = (b.wrapping_add(0x7676_7F76_767F_7676) | b) & HIGH;
        let bad_c = (c.wrapping_add(0x0076_767F) | c) & 0x0080_8080;
        if bad != 0 || bad_c != 0 {
            return BAD;
        }
        let day = byte(b, 0) * 10 + byte(b, 1);
        let hour = byte(b, 3) * 10 + byte(b, 4);
        let minute = byte(b, 6) * 10 + byte(b, 7);
        let second = byte(c as u64, 1) * 10 + byte(c as u64, 2);
        if hour > 23 || minute > 59 || second > 59 {
            return BAD;
        }
        let slot = day as usize;
        let days = if *dates.month.get_unchecked(slot) == raw {
            *dates.days.get_unchecked(slot)
        } else {
            let days = civil_day(raw, day);
            if days == BAD {
                return BAD;
            }
            dates.month[slot] = raw;
            dates.days[slot] = days;
            days
        };
        days * 86_400 + hour * 3_600 + minute * 60 + second
    }
}

/// `civil_seconds_plain`, with the day and time read as one vector.
#[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
#[inline(always)]
unsafe fn civil_seconds(p: *const u8, dates: &mut Dates) -> i64 {
    unsafe {
        use std::arch::x86_64::*;
        let raw = load64(p); // YYYY-MM-
        // "-MM-DDTHH:MM:SS" and one byte the caller checks.
        let text = _mm_loadu_si128(p.add(4) as *const __m128i);
        let template = _mm_setr_epi8(
            b'-' as i8, 0x30, 0x30, b'-' as i8, 0x30, 0x30, b'T' as i8, 0x30, 0x30, b':' as i8, 0x30, 0x30, b':' as i8,
            0x30, 0x30, 0,
        );
        let most = _mm_setr_epi8(0, 9, 9, 0, 9, 9, 0, 9, 9, 0, 9, 9, 0, 9, 9, -1);
        // Digits become 0..=9 and separators 0; anything else is over `most`.
        let digits = _mm_xor_si128(text, template);
        let over = _mm_subs_epu8(digits, most);
        let pairs = _mm_shuffle_epi8(
            digits,
            _mm_setr_epi8(4, 5, 7, 8, 10, 11, 13, 14, -1, -1, -1, -1, -1, -1, -1, -1),
        );
        // 16-bit day, hour, minute, second.
        let parts = _mm_maddubs_epi16(pairs, _mm_setr_epi8(10, 1, 10, 1, 10, 1, 10, 1, 0, 0, 0, 0, 0, 0, 0, 0));
        let late = _mm_cmpgt_epi16(parts, _mm_setr_epi16(99, 23, 59, 59, 0, 0, 0, 0));
        let wrong = _mm_or_si128(over, late);
        if _mm_testz_si128(wrong, wrong) == 0 {
            return BAD;
        }
        // 32-bit hour * 3600, then minute * 60 + second.
        let sums = _mm_cvtsi128_si64(_mm_madd_epi16(parts, _mm_setr_epi16(0, 3600, 60, 1, 0, 0, 0, 0))) as u64;
        let second = ((sums & 0xFFFF_FFFF) + (sums >> 32)) as i64;
        let day = (_mm_cvtsi128_si32(parts) & 0xFFFF) as i64;
        let slot = day as usize;
        let days = if *dates.month.get_unchecked(slot) == raw {
            *dates.days.get_unchecked(slot)
        } else {
            let days = civil_day(raw, day);
            if days == BAD {
                return BAD;
            }
            dates.month[slot] = raw;
            dates.days[slot] = days;
            days
        };
        days * 86_400 + second
    }
}

#[cfg(not(all(target_arch = "x86_64", target_feature = "avx2")))]
#[inline(always)]
unsafe fn civil_seconds(p: *const u8, dates: &mut Dates) -> i64 {
    unsafe { civil_seconds_plain(p, dates) }
}

/// Eight ASCII digits already reduced to 0..=9 per byte, first digit lowest.
#[inline(always)]
fn eight_digits(mut v: u64) -> u64 {
    const MASK: u64 = 0x0000_00FF_0000_00FF;
    const MUL1: u64 = 0x000F_4240_0000_0064;
    const MUL2: u64 = 0x0000_2710_0000_0001;
    v = v.wrapping_mul(10).wrapping_add(v >> 8);
    ((v & MASK)
        .wrapping_mul(MUL1)
        .wrapping_add(((v >> 16) & MASK).wrapping_mul(MUL2)))
        >> 32
}

/// 0 when all eight bytes of `x ^ "00000000"` are 0..=9.
#[inline(always)]
fn not_digits(t: u64) -> u64 {
    (t.wrapping_add(0x7676_7676_7676_7676) | t) & HIGH
}

#[inline(never)]
unsafe fn digits_slow(p: *const u8, len: usize) -> i64 {
    unsafe {
        if len == 0 {
            return BAD;
        }
        let mut value: i64 = 0;
        for at in 0..len {
            let digit = (*p.add(at)).wrapping_sub(b'0');
            if digit > 9 {
                return BAD;
            }
            if value > 100_000_000_000_000_000 {
                // Too long for the window, whatever follows; still not in it.
                return BAD;
            }
            value = value * 10 + digit as i64;
        }
        value
    }
}

/// Epoch milliseconds for forms F1, F2 and F3, or `BAD`.
#[inline(always)]
pub unsafe fn parse(p: *const u8, len: usize, dates: &mut Dates) -> i64 {
    unsafe {
        if len == 20 && *p.add(19) == b'Z' {
            let seconds = civil_seconds(p, dates);
            if seconds != BAD {
                return seconds * 1000;
            }
            return BAD;
        }
        if len == 13 {
            let high = load64(p) ^ 0x3030_3030_3030_3030;
            let low = load64(p.add(5)) ^ 0x3030_3030_3030_3030;
            if not_digits(high) | not_digits(low) == 0 {
                return (eight_digits(high << 24) * 100_000_000 + eight_digits(low)) as i64;
            }
            return BAD;
        }
        if len == 25 {
            let sign = *p.add(19);
            if sign == b'+' || sign == b'-' {
                let seconds = civil_seconds(p, dates);
                let c = load64(p.add(17)) ^ 0x3030_3A30_3000_3030; // SS?HH:MM, sign cleared below
                let c = c & 0xFFFF_FFFF_FF00_FFFF;
                if seconds == BAD || (c.wrapping_add(0x7676_7F76_7600_7676) | c) & HIGH != 0 {
                    return BAD;
                }
                let hours = byte(c, 3) * 10 + byte(c, 4);
                let minutes = byte(c, 6) * 10 + byte(c, 7);
                if hours > 23 || minutes > 59 {
                    return BAD;
                }
                let offset = hours * 3_600 + minutes * 60;
                return (if sign == b'+' {
                    seconds - offset
                } else {
                    seconds + offset
                }) * 1000;
            }
        }
        digits_slow(p, len)
    }
}

/// A purchase price: digits only, 1 to 10,000,000. 0 if it is not one.
#[inline(always)]
pub unsafe fn price(p: *const u8, len: usize) -> u32 {
    unsafe {
        if len == 0 {
            return 0;
        }
        let mut value: u64 = 0;
        for at in 0..len {
            let digit = (*p.add(at)).wrapping_sub(b'0');
            if digit > 9 {
                return 0;
            }
            value = value * 10 + digit as u64;
            if value > 10_000_000 {
                return 0;
            }
        }
        value as u32
    }
}

/// `prefix` then 1 to 7 digits with no leading zero: the number, else `usize::MAX`.
#[inline(always)]
pub unsafe fn numbered(p: *const u8, len: usize, prefix: &[u8]) -> usize {
    unsafe {
        let skip = prefix.len();
        if len <= skip || len > skip + 7 {
            return usize::MAX;
        }
        for (at, &expected) in prefix.iter().enumerate() {
            if *p.add(at) != expected {
                return usize::MAX;
            }
        }
        if *p.add(skip) == b'0' {
            return usize::MAX;
        }
        let mut value = 0usize;
        for at in skip..len {
            let digit = (*p.add(at)).wrapping_sub(b'0');
            if digit > 9 {
                return usize::MAX;
            }
            value = value * 10 + digit as usize;
        }
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(text: &str) -> i64 {
        let mut padded = text.as_bytes().to_vec();
        padded.extend_from_slice(&[0u8; 64]);
        unsafe { parse(padded.as_ptr(), text.len(), &mut Dates::new()) }
    }

    #[test]
    fn vector_matches_plain() {
        let mut seed = 0x1234_5678_9ABC_DEF1u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..3_000_000 {
            let mut text = *b"2027-12-19T18:13:05Z\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0";
            // Random digits, then a few random bytes anywhere.
            for at in [2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18] {
                if next() % 3 == 0 {
                    text[at] = b'0' + (next() % 10) as u8;
                }
            }
            for _ in 0..next() % 3 {
                let at = (next() % 20) as usize;
                text[at] = match next() % 4 {
                    0 => (next() % 256) as u8,
                    1 => b"0123456789:-TZ/ "[(next() % 16) as usize],
                    _ => text[at],
                };
            }
            let (fast, plain) = unsafe {
                (
                    civil_seconds(text.as_ptr(), &mut Dates::new()),
                    civil_seconds_plain(text.as_ptr(), &mut Dates::new()),
                )
            };
            assert_eq!(fast, plain, "{:?}", String::from_utf8_lossy(&text[..20]));
        }
    }

    #[test]
    fn forms() {
        assert_eq!(ts("2026-10-03T23:58:10Z"), 1_791_071_890_000);
        assert_eq!(ts("2026-10-03T19:58:10-04:00"), 1_791_071_890_000);
        assert_eq!(ts("2026-10-04T01:28:10+01:30"), 1_791_071_890_000);
        assert_eq!(ts("1791071890000"), 1_791_071_890_000);
        assert_eq!(ts("0001791071890000"), 1_791_071_890_000);
        assert_eq!(ts("00000001791071890000"), 1_791_071_890_000);
        assert_eq!(ts("0000000000001791071890000"), 1_791_071_890_000);
        assert_eq!(ts("86400000"), 86_400_000);
        assert_eq!(ts("2028-02-29T00:00:00Z"), 1_835_395_200_000);
        assert_eq!(ts("2026-10-03T23:58:10-00:00"), 1_791_071_890_000);
        assert_eq!(
            ts("2026-10-03T23:58:10+23:59"),
            1_791_071_890_000 - (23 * 3600 + 59 * 60) * 1000
        );
        for bad in [
            "",
            "2026-10-03T23:58:10.000Z",
            "2026-10-03T23:58:10z",
            "2026-10-03t23:58:10Z",
            "2026-10-03 23:58:10Z",
            "2026-10-03T23:58:10",
            "1791071890000.0",
            "1.79e12",
            "-5",
            "2026-13-03T23:58:10Z",
            "2026-02-30T23:58:10Z",
            "2027-02-29T23:58:10Z",
            "2026-10-03T24:58:10Z",
            "2026-10-03T23:58:60Z",
            "2026-10-03T23:60:10Z",
            "2026-10-03T23:58:10+24:00",
            "2026-10-03T23:58:10+05:60",
            "2026-10-03T23:58:10+0200",
            "2026-10-00T23:58:10Z",
            "2026-00-03T23:58:10Z",
            "179107189000x",
            "17910718900 0",
            "2026-10-03T23:58:10 04:00",
            "2026-10-03T23:58:1Z",
            "2026/10/03T23:58:10Z",
            "999999999999999999999",
            "2026-10-03T23:58:10+0a:00",
            "2026-10-03T23:58:10+05:0a",
            "2026-10-03T23:5a:10Z",
            "202a-10-03T23:58:10Z",
            "2026-10-03T23:58:10+05-00",
        ] {
            assert_eq!(ts(bad), BAD, "{bad:?}");
        }
    }

    #[test]
    fn cached_dates() {
        let mut dates = Dates::new();
        let mut ts = |text: &str| {
            let mut padded = text.as_bytes().to_vec();
            padded.extend_from_slice(&[0u8; 64]);
            unsafe { parse(padded.as_ptr(), text.len(), &mut dates) }
        };
        for _ in 0..3 {
            assert_eq!(ts("2026-10-03T23:58:10Z"), 1_791_071_890_000);
            assert_eq!(ts("2026-10-03T19:58:10-04:00"), 1_791_071_890_000);
            assert_eq!(ts("2027-10-03T23:58:10Z"), 1_791_071_890_000 + 365 * 86_400_000);
            assert_eq!(ts("2026-13-03T23:58:10Z"), BAD);
            assert_eq!(ts("2026-02-30T23:58:10Z"), BAD);
            assert_eq!(ts("2027-02-29T23:58:10Z"), BAD);
            assert_eq!(ts("2028-02-29T00:00:00Z"), 1_835_395_200_000);
            assert_eq!(ts("2026-1x-03T23:58:10Z"), BAD);
            assert_eq!(ts("2026-10-00T23:58:10Z"), BAD);
            assert_eq!(ts("2026-10-03T23:58:10Z"), 1_791_071_890_000);
        }
    }

    #[test]
    fn prices() {
        let p = |text: &str| {
            let mut padded = text.as_bytes().to_vec();
            padded.extend_from_slice(&[0u8; 64]);
            unsafe { price(padded.as_ptr(), text.len()) }
        };
        assert_eq!(p("0250"), 250);
        assert_eq!(p("10000000"), 10_000_000);
        assert_eq!(p("1"), 1);
        for bad in [
            "",
            "0",
            "00",
            "10000001",
            "19.99",
            "-5",
            "+500",
            "1e3",
            "250000000",
            "1000\r",
        ] {
            assert_eq!(p(bad), 0, "{bad:?}");
        }
    }
}
