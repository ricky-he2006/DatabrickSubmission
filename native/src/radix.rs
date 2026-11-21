// 并行 LSD 基数排序。对 u64 键做 8 遍 8-bit 分桶，每遍用 rayon 并行
// 统计直方图、计算前缀和、散射。内存峰值是输入长度的两倍（一份输出缓冲）。
use rayon::prelude::*;

const RADIX_BITS: u32 = 8;
const RADIX_SIZE: usize = 1 << RADIX_BITS;

/// 对 `data` 按 `key` 排序，返回排序后的新 Vec。原 Vec 被丢弃以释放内存。
pub fn par_radix_sort_by_key<T, F>(data: Vec<T>, key: F) -> Vec<T>
where
    T: Send + Sync + Copy,
    F: Fn(&T) -> u64 + Send + Sync,
{
    if data.len() < 4096 {
        let mut d = data;
        d.sort_unstable_by_key(|x| key(x));
        return d;
    }

    let n = data.len();
    let nthreads = rayon::current_num_threads();
    let chunk = (n + nthreads - 1) / nthreads;

    let mut input = data;
    let mut output: Vec<T> = vec![input[0]; n]; // T: Copy 保证可初始化

    for pass in 0..8u32 {
        let shift = pass * RADIX_BITS;

        // 每线程每桶计数
        let hist: Vec<[u32; RADIX_SIZE]> = input
            .par_chunks(chunk)
            .map(|slice| {
                let mut h = [0u32; RADIX_SIZE];
                for x in slice {
                    let b = ((key(x) >> shift) & 0xff) as usize;
                    h[b] += 1;
                }
                h
            })
            .collect();

        // 合并全局直方图，算前缀和
        let mut global = [0u32; RADIX_SIZE];
        for h in &hist {
            for i in 0..RADIX_SIZE {
                global[i] += h[i];
            }
        }
        let mut prefix = [0u32; RADIX_SIZE];
        let mut sum = 0u32;
        for i in 0..RADIX_SIZE {
            prefix[i] = sum;
            sum += global[i];
        }

        // 每线程的起始偏移 = 全局前缀 + 前面线程在该桶的计数
        let mut offsets: Vec<[u32; RADIX_SIZE]> = Vec::with_capacity(hist.len());
        let mut running = prefix;
        for h in &hist {
            offsets.push(running);
            for i in 0..RADIX_SIZE {
                running[i] += h[i];
            }
        }

        // 并行散射
        output
            .par_chunks_mut(chunk)
            .zip(input.par_chunks(chunk))
            .zip(offsets.par_iter_mut())
            .for_each(|((out_chunk, in_chunk), off)| {
                for x in in_chunk {
                    let b = ((key(x) >> shift) & 0xff) as usize;
                    let pos = off[b] as usize;
                    out_chunk[pos] = *x;
                    off[b] += 1;
                }
            });

        std::mem::swap(&mut input, &mut output);
    }
    input
}
