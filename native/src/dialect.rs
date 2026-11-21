// Identify CSV separators and quotes in sixteen bytes. Both instruction sets
// are baseline features of their respective 64-bit Linux targets.
#[cfg(target_arch = "aarch64")]
#[inline]
fn masks(bytes: &[u8; 16]) -> (u16, u16) {
    use std::arch::aarch64::*;
    unsafe {
        let input = vld1q_u8(bytes.as_ptr());
        let weights = vld1q_u8([1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128].as_ptr());
        let mask = |byte| {
            let lanes = vandq_u8(vceqq_u8(input, vdupq_n_u8(byte)), weights);
            u16::from(vaddv_u8(vget_low_u8(lanes)))
                | (u16::from(vaddv_u8(vget_high_u8(lanes))) << 8)
        };
        (mask(b','), mask(b'"'))
    }
}
#[cfg(target_arch = "x86_64")]
#[inline]
fn masks(bytes: &[u8; 16]) -> (u16, u16) {
    use std::arch::x86_64::*;
    unsafe {
        let input = _mm_loadu_si128(bytes.as_ptr().cast());
        let mask = |b| _mm_movemask_epi8(_mm_cmpeq_epi8(input, _mm_set1_epi8(b))) as u16;
        (mask(b',' as i8), mask(b'"' as i8))
    }
}
#[inline]
pub fn fields<const N: usize>(line: &[u8]) -> Option<[&[u8]; N]> {
    if !(16..=127).contains(&line.len()) {
        return super::scalar_fields(line);
    }
    let mut commas = 0u128;
    let mut quotes = 0u128;
    let mut offset = 0;
    loop {
        let (c, q) = masks(line[offset..offset + 16].try_into().unwrap());
        commas |= u128::from(c) << offset;
        quotes |= u128::from(q) << offset;
        if offset + 16 == line.len() {
            break;
        }
        offset = (offset + 16).min(line.len() - 16);
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
        if (quotes >> start) & 1 == 1 {
            if end < start + 2 || (quotes >> (end - 1)) & 1 == 0 {
                return None;
            }
            quotes ^= (1u128 << start) | (1u128 << (end - 1));
            *field = &line[start + 1..end - 1];
        } else {
            *field = &line[start..end];
        }
        start = end + 1;
    }
    (quotes == 0).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::fields;
    use crate::scalar_fields;

    // Exercise unaligned final vector loads, malformed quotes, and the
    // boundary where the scalar parser takes over.
    #[test]
    fn matches_scalar_for_all_record_lengths() {
        let mut state = 17u64;
        for len in 0..=160 {
            for _ in 0..300 {
                let mut line = vec![b'a'; len];
                for byte in &mut line {
                    state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                    *byte = match state >> 59 {
                        0 | 1 => b',',
                        2 => b'"',
                        3 => b'\r',
                        _ => b'x',
                    };
                }
                assert_eq!(fields::<2>(&line), scalar_fields::<2>(&line));
                assert_eq!(fields::<4>(&line), scalar_fields::<4>(&line));
                assert_eq!(fields::<7>(&line), scalar_fields::<7>(&line));
            }
        }
    }
}
