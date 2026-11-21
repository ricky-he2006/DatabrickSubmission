// dialect.rs — CSV field splitting with AVX-512, AVX2 and scalar fallbacks.

#[cfg(target_arch = "aarch64")]
#[inline]
fn masks16(bytes: &[u8; 16]) -> (u16, u16) {
    use std::arch::aarch64::*;
    unsafe {
        let input = vld1q_u8(bytes.as_ptr());
        let weights = vld1q_u8([1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128].as_ptr());
        let mask = |byte| {
            let lanes = vandq_u8(vceqq_u8(input, vdupq_n_u8(byte)), weights);
            u16::from(vaddv_u8(vget_low_u8(lanes))) |
                (u16::from(vaddv_u8(vget_high_u8(lanes))) << 8)
        };
        (mask(b','), mask(b'"'))
    }
}

#[cfg(target_arch = "x86_64")]
#[inline]
fn masks16(bytes: &[u8; 16]) -> (u16, u16) {
    use std::arch::x86_64::*;
    unsafe {
        let input = _mm_loadu_si128(bytes.as_ptr().cast());
        let mask = |b| _mm_movemask_epi8(_mm_cmpeq_epi8(input, _mm_set1_epi8(b))) as u16;
        (mask(b',' as i8), mask(b'"' as i8))
    }
}

#[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
#[inline]
fn masks32(bytes: &[u8; 32]) -> (u32, u32) {
    use std::arch::x86_64::*;
    unsafe {
        let input = _mm256_loadu_si256(bytes.as_ptr().cast());
        let mask = |b| _mm256_movemask_epi8(_mm256_cmpeq_epi8(input, _mm256_set1_epi8(b))) as u32;
        (mask(b',' as i8), mask(b'"' as i8))
    }
}

#[cfg(all(target_arch = "x86_64", target_feature = "avx512bw"))]
#[inline]
fn masks64(bytes: &[u8; 64]) -> (u64, u64) {
    use std::arch::x86_64::*;
    unsafe {
        let input = _mm512_loadu_si512(bytes.as_ptr().cast());
        let commas = _mm512_cmpeq_epi8_mask(input, _mm512_set1_epi8(b',' as i8));
        let quotes = _mm512_cmpeq_epi8_mask(input, _mm512_set1_epi8(b'"' as i8));
        (commas, quotes)
    }
}

#[inline]
pub fn fields<const N: usize>(line: &[u8]) -> Option<[&[u8]; N]> {
    // Very short or very long lines are rare; the scalar path avoids
    // out-of-bounds vector loads and keeps correctness simple.
    if !(16..=255).contains(&line.len()) {
        return scalar_fields(line);
    }

    let mut commas = 0u128;
    let mut quotes = 0u128;
    let mut offset = 0;

    // ── AVX-512: 64 bytes per iteration ────────────────────────────────
    #[cfg(all(target_arch = "x86_64", target_feature = "avx512bw"))]
    if line.len() >= 64 {
        loop {
            let (c, q) = masks64(line[offset..offset + 64].try_into().unwrap());
            commas |= u128::from(c) << offset;
            quotes |= u128::from(q) << offset;
            if offset + 64 >= line.len() {
                break;
            }
            offset = (offset + 64).min(line.len() - 64);
        }
    }

    // ── AVX2: 32 bytes per iteration ───────────────────────────────────
    #[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
    if line.len() >= 32 && commas == 0 && quotes == 0 {
        loop {
            let (c, q) = masks32(line[offset..offset + 32].try_into().unwrap());
            commas |= u128::from(c) << offset;
            quotes |= u128::from(q) << offset;
            if offset + 32 >= line.len() {
                break;
            }
            offset = (offset + 32).min(line.len() - 32);
        }
    }

    // ── 16-byte fallback ───────────────────────────────────────────────
    if commas == 0 && quotes == 0 {
        offset = 0;
        loop {
            let (c, q) = masks16(line[offset..offset + 16].try_into().unwrap());
            commas |= u128::from(c) << offset;
            quotes |= u128::from(q) << offset;
            if offset + 16 >= line.len() {
                break;
            }
            offset = (offset + 16).min(line.len() - 16);
        }
    }

    if commas.count_ones() != N as u32 - 1 {
        return None;
    }

    commas |= 1u128 << line.len();
    let mut start = 0;
    let mut out = [&[][..]; N];
    for field in &mut out {
        let end = commas.trailing_zeros() as usize;
        commas &= commas - 1;
        // Strip one pair of surrounding quotes if present.
        if end > start && line[start] == b'"' && line[end - 1] == b'"' {
            *field = &line[start + 1..end - 1];
        } else {
            *field = &line[start..end];
        }
        start = end + 1;
    }
    Some(out)
}

/// Scalar fallback used for lines that are too short or too long for
/// the vectorised path. It is also the correctness oracle for testing.
pub fn scalar_fields<const N: usize>(line: &[u8]) -> Option<[&[u8]; N]> {
    let mut out = [&[][..]; N];
    let mut count = 0;
    for raw in line.split(|&c| c == b',') {
        if count == N {
            return None;
        }
        let value = if raw.len() >= 2 && raw[0] == b'"' && raw[raw.len() - 1] == b'"' {
            &raw[1..raw.len() - 1]
        } else {
            raw
        };
        out[count] = value;
        count += 1;
    }
    if count == N { Some(out) } else { None }
}
