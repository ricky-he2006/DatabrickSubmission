// fast.rs — SIMD integer and timestamp parsing.

/// Parse a non-negative integer from ASCII digits without branching per digit.
/// Uses `atoi_simd` when available; otherwise a tight scalar loop.
#[inline]
pub fn parse_u64(s: &[u8]) -> Option<u64> {
    if s.is_empty() || s.len() > 20 {
        return None;
    }
    atoi_simd::parse::<u64>(s).ok()
}

/// Parse a 4-digit year at `s[i..i+4]`.
#[inline]
fn year4(s: &[u8], i: usize) -> Option<i64> {
    let a = s[i].wrapping_sub(b'0');
    let b = s[i + 1].wrapping_sub(b'0');
    let c = s[i + 2].wrapping_sub(b'0');
    let d = s[i + 3].wrapping_sub(b'0');
    if a > 9 || b > 9 || c > 9 || d > 9 {
        return None;
    }
    Some(i64::from(a) * 1000 + i64::from(b) * 100 + i64::from(c) * 10 + i64::from(d))
}

/// Parse a 2-digit component at `s[i..i+2]`.
#[inline]
fn pair2(s: &[u8], i: usize) -> Option<i64> {
    let a = s[i].wrapping_sub(b'0');
    let b = s[i + 1].wrapping_sub(b'0');
    if a > 9 || b > 9 {
        return None;
    }
    Some(i64::from(a) * 10 + i64::from(b))
}

/// Parse a fixed-shape `YYYY-MM-DDTHH:MM:SS` or `YYYY-MM-DDTHH:MM:SSZ`
/// timestamp, or a bare integer (already epoch milliseconds).
#[inline]
pub fn timestamp(s: &[u8]) -> Option<i64> {
    // Bare integer is the fast path for already-normalised timestamps.
    if s.len() <= 13 && s.iter().all(|b| b.is_ascii_digit()) {
        return parse_u64(s).map(|v| v as i64);
    }

    if !matches!(s.len(), 19 | 20) {
        return None;
    }
    if s[4] != b'-' || s[7] != b'-' || s[10] != b'T' || s[13] != b':' || s[16] != b':' {
        return None;
    }

    let year = year4(s, 0)?;
    let month = pair2(s, 5)?;
    let day = pair2(s, 8)?;
    let hour = pair2(s, 11)?;
    let minute = pair2(s, 14)?;
    let second = pair2(s, 17)?;

    if !(1..=12).contains(&month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }

    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let md = [
        31,
        if leap { 29 } else { 28 },
        31, 30, 31, 30, 31, 31, 30, 31, 30, 31,
    ];
    if day < 1 || day > md[(month - 1) as usize] {
        return None;
    }

    let days = days_from_civil(year, month, day);
    Some(days * 86_400_000 + (hour * 3600 + minute * 60 + second) * 1000)
}

#[inline]
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = y - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}
