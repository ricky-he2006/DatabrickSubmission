// Comma-split with memchr; strict quote validation per rules.md.
#[inline]
fn validate(f: &[u8]) -> Option<&[u8]> {
    if f.is_empty() { return Some(f); }
    if f.len() >= 2 && f[0] == b'"' && f[f.len() - 1] == b'"' {
        let inner = &f[1..f.len() - 1];
        if inner.iter().any(|&b| b == b'"') { return None; }
        Some(inner)
    } else if f.iter().any(|&b| b == b'"') {
        None
    } else {
        Some(f)
    }
}

pub fn fields<const N: usize>(line: &[u8]) -> Option<[&[u8]; N]> {
    if line.is_empty() { return None; }
    let mut out: [&[u8]; N] = [&[]; N];
    let mut count = 0;
    let mut start = 0;
    for pos in memchr::memchr_iter(b',', line) {
        if count >= N { return None; }
        out[count] = validate(&line[start..pos])?;
        count += 1;
        start = pos + 1;
    }
    if count >= N { return None; }
    out[count] = validate(&line[start..])?;
    count += 1;
    if count != N { return None; }
    Some(out)
}
