pub fn parse_price(s: &[u8]) -> Option<u32> {
    if s.is_empty() { return None; }
    let mut n: u64 = 0;
    for &b in s {
        if !b.is_ascii_digit() { return None; }
        n = n * 10 + (b - b'0') as u64;
        if n > 10_000_000 { return None; }
    }
    if n == 0 { return None; }
    Some(n as u32)
}

fn digits2(s: &[u8], i: usize) -> Option<i64> {
    let a = s[i].wrapping_sub(b'0');
    let b = s[i + 1].wrapping_sub(b'0');
    if a > 9 || b > 9 { return None; }
    Some(a as i64 * 10 + b as i64)
}

fn digits4(s: &[u8], i: usize) -> Option<i64> {
    let a = digits2(s, i)?;
    let b = digits2(s, i + 2)?;
    Some(a * 100 + b)
}

fn is_leap(y: i64) -> bool { y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) }

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1|3|5|7|8|10|12 => 31,
        4|6|9|11 => 30,
        2 => if is_leap(y) { 29 } else { 28 },
        _ => 0,
    }
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = y - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

fn epoch_ms(y: i64, mo: i64, d: i64, h: i64, mi: i64, s: i64) -> i64 {
    days_from_civil(y, mo, d) * 86_400_000 + (h * 3600 + mi * 60 + s) * 1000
}

fn parse_f1(s: &[u8]) -> Option<i64> {
    if s.len() != 20 || s[19] != b'Z' { return None; }
    if s[4] != b'-' || s[7] != b'-' || s[10] != b'T' || s[13] != b':' || s[16] != b':' { return None; }
    let y = digits4(s, 0)?; let mo = digits2(s, 5)?; let d = digits2(s, 8)?;
    let h = digits2(s, 11)?; let mi = digits2(s, 14)?; let se = digits2(s, 17)?;
    if !(1..=12).contains(&mo) || h > 23 || mi > 59 || se > 59 { return None; }
    if d < 1 || d > days_in_month(y, mo) { return None; }
    Some(epoch_ms(y, mo, d, h, mi, se))
}

fn parse_f2(s: &[u8]) -> Option<i64> {
    if s.len() != 25 { return None; }
    if s[4] != b'-' || s[7] != b'-' || s[10] != b'T' || s[13] != b':' || s[16] != b':' || s[22] != b':' { return None; }
    if s[19] != b'+' && s[19] != b'-' { return None; }
    let y = digits4(s, 0)?; let mo = digits2(s, 5)?; let d = digits2(s, 8)?;
    let h = digits2(s, 11)?; let mi = digits2(s, 14)?; let se = digits2(s, 17)?;
    let oh = digits2(s, 20)?; let om = digits2(s, 23)?;
    if !(1..=12).contains(&mo) || h > 23 || mi > 59 || se > 59 { return None; }
    if d < 1 || d > days_in_month(y, mo) { return None; }
    if oh > 23 || om > 59 { return None; }
    let local = epoch_ms(y, mo, d, h, mi, se);
    let sign = if s[19] == b'+' { 1 } else { -1 };
    let offset = sign * (oh * 3600 + om * 60) * 1000;
    Some(local - offset)
}

fn parse_f3(s: &[u8]) -> Option<i64> {
    if s.is_empty() { return None; }
    let mut n: i64 = 0;
    for &b in s {
        if !b.is_ascii_digit() { return None; }
        n = n.checked_mul(10)?.checked_add((b - b'0') as i64)?;
    }
    Some(n)
}

pub fn timestamp(s: &[u8]) -> Option<i64> {
    if s.len() == 20 && s[19] == b'Z' {
        parse_f1(s)
    } else if s.len() == 25 && (s[19] == b'+' || s[19] == b'-') && s[22] == b':' {
        parse_f2(s)
    } else {
        parse_f3(s)
    }
}
